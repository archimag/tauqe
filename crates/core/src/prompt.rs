use crate::context::ContextFileContent;
use crate::edits::EditProtocol;
use crate::providers::ChatMessage;
use tauqe_protocol::{ContextAccess, ModelTier, Plan, RepositoryState};

/// Default model tier used for squash commit message synthesis.
pub const SQUASH_COMMIT_TIER: ModelTier = ModelTier::Junior;

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
    prompt.push_str("2. Output ONLY the commit message text itself. Do NOT include markdown code block fences (no ```), explanations, tags, or conversational fluff.\n");

    prompt
}

pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    // Heuristic: ~4 characters per token
    text.chars().count().div_ceil(4)
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
    pub is_plan_step_execution: bool,
    pub is_discussion: bool,
    pub discovery_mode: crate::config::DiscoveryMode,
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
            is_plan_step_execution: false,
            is_discussion: false,
            discovery_mode: crate::config::DiscoveryMode::default(),
        }
    }

    pub fn with_discovery_mode(mut self, mode: crate::config::DiscoveryMode) -> Self {
        self.discovery_mode = mode;
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

    pub fn build_system_prompt(&self, protocol: &dyn EditProtocol) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are Tauqe AI, an expert programming assistant operating inside TAUQE (The Answer to the Ultimate Question of Engineering), an AI-native engineering control environment built on the literal harness paradigm.\n\n");
        prompt.push_str("## Core System Contract\n");
        prompt.push_str("1. Authoritative Source: The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.\n");
        prompt.push_str("1a. Operational Protocol Tags: All operational protocol tags (<context_request>, <doc_request>, <verify>, <user_language>, <plan>, <plan_step_done>) must be emitted strictly at the end of your response outside markdown code blocks. If you mention, quote, or discuss tags in conversational text, you MUST enclose them in backticks (e.g. `<context_request ...>`) so they are treated as literal text and not operational commands.\n");
        let marker_suffix = protocol
            .turn_marker()
            .map(|m| format!("_{}", m))
            .unwrap_or_default();

        if self.is_discussion {
            prompt.push_str("2. Discussion Mode Scope: You are strictly in Discussion mode. Code modifications, file creations, file edits, and file deletions are strictly prohibited. Do NOT output XML edit blocks (<edit>, <create>, <replace>, <overwrite>, <delete>) or any code patch proposals.\n");
            match self.discovery_mode {
                crate::config::DiscoveryMode::Monotonic => {
                    prompt.push_str(
                        "3. Read-Only Context Exploration: Files in <read_only_files> are strictly for reference and architectural understanding. If you need to inspect additional files from the repository map to answer the developer's questions, specify them in the `context_requests` array with `access: \"read_only\"`.\n",
                    );
                }
                crate::config::DiscoveryMode::Incremental => {
                    prompt.push_str(
                        "3. Read-Only Context Exploration (Incremental Strategy): Files in <read_only_files> are strictly for reference and architectural understanding. If you need to inspect additional files from the repository map to answer the developer's questions, specify them in the `context_requests` array with `access: \"read_only\"`. If an inspected auto-context file turns out to be irrelevant, specify it in `context_drops` to release context and preserve token budget.\n",
                    );
                }
                crate::config::DiscoveryMode::Snapshot => {
                    prompt.push_str(
                        "3. Read-Only Context Exploration (Snapshot Strategy): Files in <read_only_files> are strictly for reference and architectural understanding. In each discovery round, specify the complete active working set of auto-context files needed for your reasoning in `context_requests` (`access: \"read_only\"`). Any auto-context files omitted from `context_requests` will be automatically evicted from context. User and pinned files are never evicted.\n",
                    );
                }
            }
        } else {
            prompt.push_str(&format!(
                "2. Read-Only Scope: Files inside <read_only_files> are strictly for reference and understanding. Do NOT propose edits to them directly. If completing the user's task requires modifying a file currently in <read_only_files>, request editable access first using <context_request{} path=\"...\" access=\"editable\" />.\n",
                marker_suffix
            ));
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
        prompt.push_str(&format!(
            "9. Code Verification: You can trigger deterministic code verification (check, clippy, test) using <verify{} target=\"all|check|test|clippy\" on_success=\"silent|report\" />. Always use on_success=\"silent\" unless the user explicitly requested to see raw command logs or test output. Verification is primarily for self-checking; never dump or quote full test/build logs if verification succeeds. A concise confirmation that the code is verified is sufficient.\n",
            marker_suffix
        ));
        prompt.push_str(&format!(
            "10. Self-Knowledge: When the user asks about your identity, capabilities, concepts, workflows, keyboard shortcuts, or configuration, do NOT guess. Request the authoritative documentation using <doc_request{} topic=\"{}\" /> and output nothing else except a short note. You will be called again with <system_documentation> blocks; base your answer strictly on them and formulate it in the user's language.\n",
            marker_suffix,
            crate::docs::TOPICS.join("|")
        ));
        if !self.is_plan_step_execution {
            if self.is_discussion {
                prompt.push_str(
                    "11. Local Engineering Plans: When formulating or updating a task plan, populate the structured `plan_update` field in your response object with `action: \"save\"|\"update\"|\"delete\"`, plan `id`, `title`, `description`, and `items`. Plan Action Semantics: Use `action: \"save\"` to create or completely overwrite/replace the entire step tree (e.g. initial plan creation, full tree restructuring, or clearing all items); use `action: \"update\"` strictly to modify specific individual nodes or items in place by ID without touching unmentioned items; use `action: \"delete\"` to remove the plan. Reins Invariant: Never mutate the plan (leave `plan_update: null`) without an explicit user instruction to modify it. Questions, analysis, trade-off reviews, and critique must be answered strictly in conversational text in `message`. Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive step tree without premature low-level code churn or deep file inspection. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Each item has `status: \"todo\"|\"discussion\"|\"in_progress\"|\"done\"|\"cancelled\"`. If you have architectural doubts, alternative approaches, or decisions requiring developer clarification, explicitly set status to \"discussion\". Always provide a clear `description` explaining the overall architecture, boundaries, and rationale. Granularity Contract: Each leaf step is executed in an isolated turn with compulsory toolchain verification, so calibrate leaf step granularity to cohesive functional units of work. Language Consistency: Formulate plan title, description, step titles, and step details in the same language as the plan description and user's request. In <active_plan>, items marked with [x] are focused by the user.\n\n",
                );
            } else {
                prompt.push_str(&format!(
                    "11. Local Engineering Plans: When formulating a multi-step task plan, wrap structured plan items in <plan{0} action=\"save\" id=\"plan-id\" title=\"Plan Title\">...</plan{0}>. In Develop mode, you may ONLY propose new plans (action=\"save\"). Updating plan statuses, closing steps, or modifying existing plans from Develop mode is strictly prohibited (step execution and completion occur strictly in isolated step turns, while plan modifications and re-scoping belong exclusively in Discussion mode). Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What'), aligning on key architectural decisions and trade-offs, and defining a cohesive step tree without premature low-level code churn or deep file inspection. Step Formulation Contract: Formulate each step around an engineering objective and operational invariants rather than detailed procedural micro-instructions; define what state must be achieved and what invariants preserved. Each item has status=\"todo|discussion\". If you have architectural doubts, multiple technical options, or items needing developer clarification, explicitly set status=\"discussion\" to flag them for review before execution. Always provide a clear <description> explaining the overall architecture, boundaries, and rationale, because plan steps are executed in isolated turns without chat history. Granularity Contract: Each leaf step is executed in an isolated turn with compulsory toolchain verification (check, clippy, test). Therefore, calibrate leaf step granularity to cohesive, self-contained functional units of work. Avoid micro-fragmentation as well as unmanageable monoliths. Plan Creation Invariant: Whenever you propose a new plan, you MUST precede the tag with a concise conversational explanation of the proposed plan, and you MUST emit the structured <plan{0} action=\"save\" ...> tag in the same response. Language Consistency: Formulate the plan title, description, step titles, and step details in the same language as the plan description and user's request.\n\n",
                    marker_suffix
                ));
            }
        } else {
            prompt.push('\n');
        }

        if !self.is_discussion {
            let editable_paths: Vec<String> = self
                .context_files
                .iter()
                .filter(|f| f.access == ContextAccess::Editable)
                .map(|f| f.path.clone())
                .collect();

            // Delegate edit protocol specific instructions
            prompt.push_str(&protocol.system_instructions(&editable_paths, self.discovery_mode));
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

    pub fn assemble_chat_messages(
        &self,
        current_prompt: &str,
        protocol: &dyn EditProtocol,
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

        if let Some(history_block) = self.format_history_block() {
            system_text.push_str("## Session History\n");
            system_text.push_str(&history_block);
            system_text.push_str("\n\n");
        }

        if !self.is_plan_step_execution {
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
    use crate::edits::XmlEditProtocol;

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
        let proto = XmlEditProtocol;
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
        let proto = XmlEditProtocol;
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
        let proto = XmlEditProtocol;
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
        assert_eq!(estimate_tokens("абвг"), 1);
    }

    #[test]
    fn test_planning_prompt_instructions() {
        let proto = XmlEditProtocol;

        // Develop mode
        let dev_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        let dev_prompt = dev_assembly.build_system_prompt(&proto);
        assert!(dev_prompt.contains("Planning Objective: Plan synthesis focuses on conceptual comprehension of the objective ('What')"));
        assert!(dev_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));

        // Discussion mode
        let mut disc_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None);
        disc_assembly.is_discussion = true;
        let disc_prompt = disc_assembly.build_system_prompt(&proto);
        assert!(disc_prompt.contains("Planning Objective: Collaborative planning focuses on conceptual comprehension of the objective ('What')"));
        assert!(disc_prompt.contains("Step Formulation Contract: Formulate each step around an engineering objective and operational invariants"));
    }

    #[test]
    fn test_prompt_assembly_discovery_mode_instructions() {
        let proto = XmlEditProtocol;

        // Monotonic Develop mode (default)
        let mono_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Monotonic);
        let mono_prompt = mono_assembly.build_system_prompt(&proto);
        assert!(!mono_prompt.contains("<context_drop"));
        assert!(!mono_prompt.contains("Incremental Strategy"));
        assert!(!mono_prompt.contains("Snapshot Strategy"));

        // Incremental Develop mode
        let inc_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Incremental);
        let inc_prompt = inc_assembly.build_system_prompt(&proto);
        assert!(inc_prompt.contains("<context_drop"));
        assert!(inc_prompt.contains("Incremental Strategy"));
        assert!(!inc_prompt.contains("Snapshot Strategy"));

        // Snapshot Develop mode
        let snap_assembly = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Snapshot);
        let snap_prompt = snap_assembly.build_system_prompt(&proto);
        assert!(snap_prompt.contains("Snapshot Strategy"));
        assert!(snap_prompt.contains("entire active working set of auto-context files"));

        // Discussion mode monotonic
        let mut disc_mono = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Monotonic);
        disc_mono.is_discussion = true;
        let disc_mono_prompt = disc_mono.build_system_prompt(&proto);
        assert!(!disc_mono_prompt.contains("context_drops"));

        // Discussion mode incremental
        let mut disc_inc = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Incremental);
        disc_inc.is_discussion = true;
        let disc_inc_prompt = disc_inc.build_system_prompt(&proto);
        assert!(disc_inc_prompt.contains("context_drops"));

        // Discussion mode snapshot
        let mut disc_snap = PromptAssembly::new(None, 1, vec![], "git", "xml", None, None)
            .with_discovery_mode(crate::config::DiscoveryMode::Snapshot);
        disc_snap.is_discussion = true;
        let disc_snap_prompt = disc_snap.build_system_prompt(&proto);
        assert!(disc_snap_prompt.contains("Snapshot Strategy"));
    }
}
