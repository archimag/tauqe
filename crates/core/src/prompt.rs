pub mod json;
pub mod protocol;
pub mod stream;
pub mod xml;

use crate::context::ContextFileContent;
use crate::edits::stream::json::decode::first_json_object_str;
use crate::prompt::protocol::PromptProtocol;
use crate::providers::{ChatMessage, JsonSchemaDefinition, ResponseFormat};
use tauqe_protocol::{ContextAccess, ModelRole, Plan, RepositoryState};

/// Default model role used for squash commit message synthesis.
pub const SQUASH_COMMIT_ROLE: ModelRole = ModelRole::Scribe;

/// Maximum characters allowed in a turn scratchpad block before safe truncation.
pub const MAX_SCRATCHPAD_CHARS: usize = 4_000;

pub fn build_squash_commit_prompt(
    commits: &[crate::git::CommitSummary],
    cumulative_diff: &str,
    pinned_files: &[ContextFileContent],
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an expert developer assistant. Your task is to write a single high-quality commit message for squashing a series of commits into one.\n\n");

    if !pinned_files.is_empty() {
        prompt.push_str("## Project Guidelines & Context\n");
        prompt.push_str("Follow any commit message conventions, scopes, and guidelines specified below:\n");
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

pub struct PromptAssembly {
    pub repo_state: Option<RepositoryState>,
    pub context_revision: u64,
    pub context_files: Vec<ContextFileContent>,
    pub workflow: String,
    pub edit_protocol: String,
    pub history_tag: Option<String>,
    pub repo_map: Option<String>,
    pub target_language: Option<String>,
    pub active_plan: Option<Plan>,
    pub is_isolated_step: bool,
    pub is_discussion: bool,
    pub turn_scratchpad: Option<String>,
}

impl PromptAssembly {
    pub fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
        workflow: impl Into<String>,
        edit_protocol: impl Into<String>,
        history_tag: Option<String>,
        repo_map: Option<String>,
    ) -> Self {
        Self {
            repo_state,
            context_revision,
            context_files,
            workflow: workflow.into(),
            edit_protocol: edit_protocol.into(),
            history_tag,
            repo_map,
            target_language: None,
            active_plan: None,
            is_isolated_step: false,
            is_discussion: false,
            turn_scratchpad: None,
        }
    }

    pub fn with_isolated_step(mut self, is_isolated: bool) -> Self {
        self.is_isolated_step = is_isolated;
        self
    }

    pub fn with_scratchpad(mut self, scratchpad: Option<String>) -> Self {
        self.turn_scratchpad = scratchpad;
        self
    }

    pub fn with_discussion(mut self, is_discussion: bool) -> Self {
        self.is_discussion = is_discussion;
        self
    }

    pub fn with_active_plan(mut self, plan: Option<Plan>) -> Self {
        self.active_plan = plan;
        self
    }

    pub fn with_target_language(mut self, target_language: Option<String>) -> Self {
        self.target_language = target_language;
        self
    }

    pub fn build_system_prompt(&self, protocol: &dyn PromptProtocol) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are Tauqe AI, an expert programming assistant operating inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n\n");
        prompt.push_str("## Core System Contract\n");
        prompt.push_str("1. Authoritative Source: The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.\n");
        if self.is_isolated_step {
            prompt.push_str("1a. Operational Protocol Tags: All operational protocol tags (<context_request>, <doc_request>, <user_language>, <scratchpad>, <success>, <error>, <blocked>) must be emitted strictly at the end of your response outside markdown code blocks. If you mention, quote, or discuss tags in conversational text, you MUST enclose them in backticks (e.g. `<context_request ...>`) so they are treated as literal text and not operational commands.\n");
        } else {
            prompt.push_str("1a. Operational Protocol Tags: All operational protocol tags (<context_request>, <doc_request>, <verify>, <user_language>, <plan>, <scratchpad>, <success>, <error>, <blocked>) must be emitted strictly at the end of your response outside markdown code blocks. If you mention, quote, or discuss tags in conversational text, you MUST enclose them in backticks (e.g. `<context_request ...>`) so they are treated as literal text and not operational commands.\n");
        }
        let marker_suffix = protocol
            .turn_marker()
            .map(|m| format!("_{}", m))
            .unwrap_or_default();

        if self.is_discussion {
            prompt.push_str("2. Discussion Mode Scope: You are strictly in Discussion mode. Code modifications, file creations, file edits, and file deletions are strictly prohibited. Do NOT output XML edit blocks (<edit>, <create>, <replace>, <overwrite>, <delete>) or any code patch proposals.\n");
            prompt.push_str(
                "3. Read-Only Context Exploration: Files in <read_only_files> are strictly for reference and architectural understanding. If you need to inspect additional files from the repository map to answer the developer's questions, specify them in the `context_requests` array with `access: \"read_only\"`.\n",
            );
        } else {
            prompt.push_str(
                "2. Read-Only Scope: Files inside <read_only_files> are provided for reference, context, and structural comprehension. Prefer modifying files in <editable_files>. If completing the task requires editing a file present in <read_only_files>, you may propose edits to it directly without an extra discovery round — the environment will automatically promote it to editable upon applying the changes.\n"
            );
            prompt.push_str("3. Editable Scope: Files inside <editable_files> are permitted for modification. You may also create new files using <create path=\"...\"> or move/rename files using <move from=\"...\" to=\"...\" /> when required by the task.\n");
        }
        prompt.push_str("4. No Arbitrary Shell: You do not have shell execution capabilities. Work strictly through the context and actions provided.\n");
        if !self.is_discussion {
            prompt.push_str("5. Minimal Coherent Change: Prefer the smallest coherent modification necessary to complete the task.\n");
            prompt.push_str("6. Always Explain Changes: Whenever you propose file edits, you MUST precede them with a concise conversational explanation (1-3 sentences) explaining what changes were made, why, and how they achieve the user's intent. Never output edits alone without an accompanying explanation.\n");
        }
        prompt.push_str("7. Reasoning in English: Always conduct internal reasoning, planning, and thinking strictly in English to preserve token budget and maximize reasoning quality.\n");

        if let Some(lang) = &self.target_language {
            prompt.push_str(&format!(
                "8. Language Consistency: The user's language is {}. All conversational explanations, answers, and messages MUST be in {}. Do NOT switch to English for conversational responses unless requested.\n",
                lang, lang
            ));
        } else if self.is_discussion {
            prompt.push_str(
                "8. Language Consistency: Always formulate conversational explanations and user-facing answers in `message` in the same language as the user's request. Identify the user's language in the `user_language` field (e.g. \"Russian\"). Do NOT switch to English for conversational responses unless requested.\n",
            );
        } else {
            prompt.push_str(&format!(
                "8. Language Consistency: Always formulate conversational explanations and user-facing answers in the same language as the user's request. Identify the user's language using <user_language{}>language_name</user_language{}> (e.g. <user_language{}>Russian</user_language{}>). Do NOT switch to English for conversational responses unless requested.\n",
                marker_suffix, marker_suffix, marker_suffix, marker_suffix
            ));
        }
        if self.is_isolated_step {
            prompt.push_str(
                "9. Code Verification: Verification on code modifications is handled automatically and deterministically by the harness at the end of each step. Do NOT emit <verify> tags (the harness runs toolchain verification and gating automatically).\n"
            );
        } else {
            prompt.push_str(&format!(
                "9. Code Verification: Verification on code modifications is performed automatically and deterministically by the harness. Emit <verify{0} target=\"all|check|test|clippy\" on_success=\"silent|report\" /> ONLY when the user explicitly requests running verification, compiler checks, or tests to inspect codebase status. Always use on_success=\"silent\" unless the user explicitly asked to see raw command logs or test output. Do NOT emit <verify> on normal code edits, as the harness verifies them automatically.\n",
                marker_suffix
            ));
        }
        prompt.push_str(&format!(
            "10. Self-Knowledge: When the user asks about your identity, capabilities, concepts, workflows, keyboard shortcuts, or configuration, do NOT guess. Request the authoritative documentation using <doc_request{} topic=\"{}\" /> and output nothing else except a short note. You will be called again with <system_documentation> blocks; base your answer strictly on them and formulate it in the user's language.\n",
            marker_suffix,
            crate::docs::topic_hint()
        ));
        prompt.push_str(&format!(
            "10a. Turn Scratchpad (Working Memory): To retain working hypotheses, verified findings, or focus across multi-round discovery and verification healing cycles, you may emit <scratchpad{0}>...</scratchpad{0}>. Keep scratchpad notes concise (bullet points of what is verified, current hypothesis, and purpose of requested files). Notes are preserved within the current turn in <scratchpad> and isolated from persistent history.\n",
            marker_suffix
        ));
        if !self.is_isolated_step {
            if self.is_discussion {
                prompt.push_str(
                    "11. Local Engineering Plans: When formulating or updating a task plan, populate the structured `plan_update` field in your response object with `action: \"save\"|\"update\"|\"delete\"`, plan `id`, `title`, `description`, `concept`, and `items`. Conceptual Foundation Invariant: Every plan in TAUQE is a two-section document and MUST have an architectural concept tree in `concept` defining requirements, design decisions, and system invariants ('What'). When creating a new plan (`action: \"save\"`), you MUST populate `concept` with at least one concept node. Creating a plan without a concept tree is strictly prohibited. Implementation steps in `items` ('How') are either outlined with `concept_refs` pointing to concept nodes, or left empty (`items: []`) to be synthesized subsequently by deep refinement (`plan_refine`). File Attributes Contract: `files` lists target files to create or modify (loaded as editable context), while `readonly_files` lists reference or contract files (loaded as read-only context and never modified). Leaf-Only Files Invariant: `files` and `readonly_files` must be specified exclusively for executable leaf steps without children; parent container items and task groups must have an empty `files: []` and `readonly_files: []` lists. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite/replace the entire plan; use `action: \"update\"` strictly to modify specific individual nodes or items in place by ID without touching unmentioned items; use `action: \"delete\"` to remove the plan. Reins Invariant: Never mutate the plan (leave `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`. Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants; define what state must be achieved and what invariants preserved. Always provide a clear `description` explaining the overall architecture, boundaries, and rationale. Language Consistency: Formulate plan title, description, concept titles/details, and step titles/details in the same language as the plan description and user's request. In <active_plan>, items marked with [FOCUSED] are focused by the user.\n\n",
                );
            } else {
                prompt.push_str(&format!(
                    "11. Local Engineering Plans: When formulating a task plan, wrap structured plan items in <plan{0} action=\"save\" id=\"plan-id\" title=\"Plan Title\">...</plan{0}>. In Develop mode, you may ONLY propose new plans (action=\"save\"). Updating plan statuses, closing steps, or modifying existing plans from Develop mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Conceptual Foundation Invariant: Every plan MUST contain an architectural concept tree in <concept>...</concept> defining the core requirements, design decisions, and system invariants ('What'). Inside <concept>, define architectural nodes using structured <concept_item id=\"c-...\" title=\"...\"><details>...</details></concept_item> tags. Plan creation without <concept> containing at least one <concept_item> is strictly prohibited. Implementation steps in <item> define 'How', may be included with concept_refs=\"c-...\", files=\"...\", and readonly=\"...\", or omitted if awaiting deep refinement. File Attributes Contract: the `files` attribute lists target files to create or modify (loaded as editable context), while the `readonly` attribute lists reference or contract files such as subsystem conventions, protocol types, and interfaces (loaded as read-only context and never modified). Leaf-Only Files Invariant: the `files` and `readonly` attributes must be specified exclusively for executable leaf steps without child items; parent container items and task groups must never have files. Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive concept tree without premature low-level code churn or deep file inspection. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Always provide a clear <description> explaining the overall architecture, boundaries, and rationale. Plan Creation Invariant: Whenever you propose a new plan, you MUST precede the tag with a concise conversational explanation of the proposed plan, and you MUST emit the structured <plan{0} action=\"save\" ...> tag in the same response containing both <concept> (mandatory) and optional <item> elements. Language Consistency: Formulate the plan title, description, concept nodes, step titles, and step details in the same language as the user's request.\nStructured Plan XML Format:\n<plan{0} action=\"save\" id=\"plan-id\" title=\"Plan Title\">\n  <description>Overall architecture and rationale</description>\n  <concept>\n    <concept_item id=\"c-1\" title=\"Core Invariant\">\n      <details>Design rationale and invariants</details>\n    </concept_item>\n  </concept>\n  <item id=\"1\" title=\"Step Title\" concept_refs=\"c-1\" files=\"path/to/file.rs\" readonly=\"path/to/contract.rs\">\n    <details>Engineering objective and verification criteria</details>\n  </item>\n</plan{0}>\n\n",
                    marker_suffix
                ));
            }
        } else {
            prompt.push_str("11. Step Execution Scope: You are executing a single isolated plan step. Plan creation, modification, status updates, or plan tags (<plan>) are strictly prohibited (step lifecycle and plan tracking are handled deterministically by the harness).\n\n");
        }

        if !self.is_discussion {
            let editable_paths: Vec<String> = self
                .context_files
                .iter()
                .filter(|f| f.access == ContextAccess::Editable)
                .map(|f| f.path.clone())
                .collect();

            // Delegate edit protocol specific instructions
            prompt.push_str(&protocol.system_instructions(&editable_paths));
        } else {
            prompt.push_str("## Mode: Structured Discussion\n");
            prompt.push_str("You are participating in an architectural discussion. Code modifications are strictly prohibited. You cannot edit, create, or delete code files. Focus on conceptual comprehension of the objective ('What'), clarifying requirements, evaluating trade-offs, establishing key design decisions, and formulating or refining plans via structured fields.\n");
            prompt.push_str("Output MUST be a JSON object conforming to the discussion schema with `message`, `plan_update`, `context_requests`, and `user_language`.\n\n");
        }

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

    pub fn format_context_block(&self) -> Option<String> {
        if self.context_files.is_empty() {
            return None;
        }

        let mut out = String::new();
        out.push_str(&format!(
            "<context revision=\"{}\">\n",
            self.context_revision
        ));

        if self.is_discussion {
            // In discussion mode, all context files are rendered strictly as read-only
            out.push_str("  <read_only_files>\n");
            for f in &self.context_files {
                out.push_str(&format!("    <file path=\"{}\">\n", f.path));
                out.push_str(&f.content);
                if !f.content.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("    </file>\n");
            }
            out.push_str("  </read_only_files>\n");
        } else {
            let ro_files: Vec<_> = self
                .context_files
                .iter()
                .filter(|f| f.access == ContextAccess::ReadOnly)
                .collect();

            if !ro_files.is_empty() {
                out.push_str("  <read_only_files>\n");
                for f in ro_files {
                    out.push_str(&format!("    <file path=\"{}\">\n", f.path));
                    out.push_str(&f.content);
                    if !f.content.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("    </file>\n");
                }
                out.push_str("  </read_only_files>\n");
            }

            let ed_files: Vec<_> = self
                .context_files
                .iter()
                .filter(|f| f.access == ContextAccess::Editable)
                .collect();

            if !ed_files.is_empty() {
                out.push_str("  <editable_files>\n");
                for f in ed_files {
                    out.push_str(&format!("    <file path=\"{}\">\n", f.path));
                    out.push_str(&f.content);
                    if !f.content.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("    </file>\n");
                }
                out.push_str("  </editable_files>\n");
            }
        }

        out.push_str("</context>");
        Some(out)
    }

    pub fn format_history_block(&self) -> Option<String> {
        self.history_tag.as_ref().and_then(|tag| {
            let trimmed = tag.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
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

    pub fn format_scratchpad_block(&self) -> Option<String> {
        self.turn_scratchpad.as_ref().and_then(|pad| {
            let trimmed = pad.trim();
            if trimmed.is_empty() {
                None
            } else {
                let text = if trimmed.chars().count() > MAX_SCRATCHPAD_CHARS {
                    let truncated: String =
                        trimmed.chars().take(MAX_SCRATCHPAD_CHARS).collect();
                    format!(
                        "{}\n... [scratchpad truncated for length] ...",
                        truncated
                    )
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
        let mut messages = Vec::new();

        // System prompt + Project metadata + Authoritative context + Repo map + Session history + Protocol instructions
        let mut system_text = self.build_system_prompt(protocol);
        if let Some(context_block) = self.format_context_block() {
            system_text.push_str("## Project Context\n");
            system_text.push_str(&context_block);
            system_text.push_str("\n\n");
        }

        if let Some(repo_map_block) = self.format_repo_map_block() {
            system_text.push_str("## Repository Map\n");
            system_text.push_str(&repo_map_block);
            system_text.push_str("\n\n");
        }

        if let Some(scratchpad_block) = self.format_scratchpad_block() {
            system_text.push_str("## Turn Scratchpad (Working Memory)\n");
            system_text.push_str(&scratchpad_block);
            system_text.push_str("\n\n");
        }

        if let Some(history_block) = self.format_history_block() {
            system_text.push_str("## Session History\n");
            system_text.push_str(&history_block);
            system_text.push_str("\n\n");
        }

        if !self.is_isolated_step {
            if let Some(plan) = &self.active_plan {
                if let Some(plan_block) = crate::plan::format_active_plan_context(plan) {
                    system_text.push_str("## Active Task Plan Context\n");
                    system_text.push_str(&plan_block);
                    system_text.push_str("\n\n");
                }
            }
        }

    
        if let Some(target_lang) = &self.target_language {
            system_text.push_str("## Language Directive\n");
            system_text.push_str(&format!("- Target Response Language: {} (all conversational explanations, answers, messages, and plan items/titles/details MUST be in {})\n", target_lang, target_lang));
            system_text.push_str("- Reasoning / Thoughts: Strictly in English.\n\n");
        }

        messages.push(ChatMessage::system(system_text));

        // Current user request
        messages.push(ChatMessage::user(current_prompt));

        messages
    }

}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        assembly.target_language = Some("Russian".to_string());
        let proto = XmlPromptProtocol;
        let messages = assembly.assemble_chat_messages("Привет", &proto);
        assert!(messages[0].content.contains("## Language Directive"));
        assert!(messages[0].content.contains("Target Response Language: Russian"));
        assert!(messages[0].content.contains("Reasoning / Thoughts: Strictly in English."));
    }

    #[test]
    fn test_prompt_assembly() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let history = Some("<history>\n{\"type\":\"summary\",\"text\":\"Initial\"}\n</history>".to_string());
        let repo_map = Some("crates/core/src/git.rs\n  pub fn get_repository_state()".to_string());
        let assembly = PromptAssembly::new(None, 1, files, "git", "xml", history, repo_map);
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
        assert!(messages[0].content.contains("{\"type\":\"summary\",\"text\":\"Initial\"}"));
        assert_eq!(messages[1], ChatMessage::user("Explain this code"));
    }

    
    #[test]
    fn test_prompt_assembly_discussion_mode() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let mut assembly = PromptAssembly::new(None, 1, files, "git", "xml", None, None);
        assembly.is_discussion = true;
        let proto = XmlPromptProtocol;
        let messages = assembly.assemble_chat_messages("Let's discuss architecture", &proto);
        let sys = &messages[0].content;

        assert!(sys.contains("Discussion Mode Scope"));
        assert!(sys.contains("Mode: Structured Discussion"));
        assert!(!sys.contains("<editable_files>"));
        assert!(sys.contains("<read_only_files>"));
        assert!(!sys.contains("Code Modification Protocol"));
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
        let mut assembly = PromptAssembly::new(
            None,
            1,
            vec![],
            "git",
            "xml",
            None,
            None,
        );
        assembly.is_isolated_step = true;
        assembly.active_plan = Some(Plan {
            id: "plan-test".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            concept: vec![],
            items: vec![],
        });

        let sys_prompt = assembly.build_system_prompt(&proto);
        assert!(!sys_prompt.contains("11. Local Engineering Plans"));
        assert!(sys_prompt.contains("11. Step Execution Scope"));

        let messages = assembly.assemble_chat_messages("Execute step", &proto);
        assert!(!messages[0].content.contains("## Active Task Plan Context"));
    }

    #[test]
    fn test_planning_prompt_instructions() {
        let proto = XmlPromptProtocol;

        // Develop mode
        let dev_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        let dev_prompt = dev_assembly.build_system_prompt(&proto);
        assert!(dev_prompt.contains("Conceptual Foundation Invariant"));
        assert!(dev_prompt.contains("<concept_item"));
        assert!(dev_prompt.contains("Structured Plan XML Format"));
        assert!(dev_prompt.contains("Leaf-Only Files Invariant"));
        assert!(dev_prompt.contains("Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What')"));
        assert!(dev_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));
        assert!(dev_prompt.contains("File Attributes Contract"));
        assert!(dev_prompt.contains("readonly"));

        // Discussion mode
        let mut disc_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        disc_assembly.is_discussion = true;
        let disc_prompt = disc_assembly.build_system_prompt(&proto);
        assert!(disc_prompt.contains("Conceptual Foundation Invariant"));
        assert!(disc_prompt.contains("Leaf-Only Files Invariant"));
        assert!(disc_prompt.contains("Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What')"));
        assert!(disc_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));
    }

    #[test]
    fn test_prompt_assembly_discovery_instructions() {
        let proto = XmlPromptProtocol;

        let assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        let prompt = assembly.build_system_prompt(&proto);
        assert!(!prompt.contains("<context_drop"));
        assert!(!prompt.contains("Incremental Strategy"));
        assert!(!prompt.contains("Snapshot Strategy"));
        assert!(prompt.contains("<context_request"));
    }

    #[test]
    fn test_prompt_assembly_with_turn_scratchpad() {
        let proto = XmlPromptProtocol;
        let mut assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad = Some("   ".to_string());
        assert!(assembly.format_scratchpad_block().is_none());

        assembly.turn_scratchpad = Some("- Hypothesis: Foo needs Bar\n- Status: Inspecting bar.rs".to_string());
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("<scratchpad>"));
        assert!(block.contains("- Hypothesis: Foo needs Bar"));
        assert!(block.contains("</scratchpad>"));

        let messages = assembly.assemble_chat_messages("Continue", &proto);
        assert!(messages[0].content.contains("## Turn Scratchpad (Working Memory)"));
        assert!(messages[0].content.contains("- Hypothesis: Foo needs Bar"));
    }

    #[test]
    fn test_turn_scratchpad_truncation_utf8_safety() {
        let multi_byte_text = "Рабочая гипотеза и детали проверки UTF-8 символов. ".repeat(200);
        assert!(multi_byte_text.len() > MAX_SCRATCHPAD_CHARS);

        let mut assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        assembly.turn_scratchpad = Some(multi_byte_text);
        let block = assembly.format_scratchpad_block().unwrap();
        assert!(block.contains("... [scratchpad truncated for length] ..."));
    }

    #[test]
    fn test_prompt_assembly_renders_protocol_instructions() {
        use crate::prompt::json::JsonPromptProtocol;

        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let assembly = PromptAssembly::new(None, 1, files, "git", "xml", None, None);

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
        assert_eq!(messages[1], ChatMessage::user("Explain this code"));
    }

    #[test]
    fn test_prompt_assembly_preserves_turn_marker() {
        let assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        let marked = XmlPromptProtocol
            .with_turn_marker("M12345")
            .expect("marker-bound protocol");

        let prompt = assembly.build_system_prompt(&*marked);
        assert!(prompt.contains("<scratchpad_M12345>"));
        assert!(prompt.contains("</scratchpad_M12345>"));

        let messages = assembly.assemble_chat_messages("Go", &*marked);
        assert!(messages[0].content.contains("<scratchpad_M12345>"));
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
        assert_eq!(parse_compaction_summary(""), None);
        assert_eq!(parse_compaction_summary("{\"summary\":\"   \"}"), None);
        assert_eq!(parse_squash_commit_message("not json"), None);
    }

    #[test]
    fn test_parse_structured_scribe_output_is_utf8_safe() {
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
}
