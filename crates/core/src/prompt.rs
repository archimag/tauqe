use crate::context::ContextFileContent;
use crate::edits::EditProtocol;
use crate::providers::ChatMessage;
use tauqe_protocol::{ContextAccess, Plan, RepositoryState, ReviewItem};

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
    pub active_review_findings: Vec<ReviewItem>,
    pub active_plan: Option<Plan>,
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
            active_review_findings: Vec::new(),
            active_plan: None,
        }
    }

    pub fn with_active_plan(mut self, plan: Option<Plan>) -> Self {
        self.active_plan = plan;
        self
    }

    pub fn with_active_review_findings(mut self, findings: Vec<ReviewItem>) -> Self {
        self.active_review_findings = findings;
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
        let marker_suffix = protocol
            .turn_marker()
            .map(|m| format!("_{}", m))
            .unwrap_or_default();

        prompt.push_str(&format!(
            "2. Read-Only Scope: Files inside <read_only_files> are strictly for reference and understanding. Do NOT propose edits to them directly. If completing the user's task requires modifying a file currently in <read_only_files>, request editable access first using <context_request{} path=\"...\" access=\"editable\" />.\n",
            marker_suffix
        ));
        prompt.push_str("3. Editable Scope: Files inside <editable_files> are permitted for modification. You may also create new files using <create path=\"...\"> or move/rename files using <move from=\"...\" to=\"...\" /> when required by the task.\n");
        prompt.push_str("4. No Arbitrary Shell: You do not have shell execution capabilities. Work strictly through the context and actions provided.\n");
        prompt.push_str("5. Minimal Coherent Change: Prefer the smallest coherent modification necessary to complete the task.\n");
        prompt.push_str("6. Always Explain Changes: Whenever you propose file edits, you MUST precede them with a concise conversational explanation (1-3 sentences) explaining what changes were made, why, and how they achieve the user's intent. Never output edits alone without an accompanying explanation.\n");
        prompt.push_str("7. Reasoning in English: Always conduct internal reasoning, planning, and thinking strictly in English to preserve token budget and maximize reasoning quality.\n");

        if let Some(lang) = &self.target_language {
            prompt.push_str(&format!(
                "8. Language Consistency: The user's language is {}. All conversational explanations, answers, and messages MUST be in {}. Do NOT switch to English for conversational responses unless requested.\n",
                lang, lang
            ));
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
        prompt.push_str(&format!(
            "11. Local Engineering Plans: When formulating or updating a multi-step task plan, wrap structured plan items in <plan{0} action=\"save|update\" id=\"plan-id\" title=\"Plan Title\">...</plan{0}>. In <active_plan>, items marked with [x] are focused by the user; concentrate your efforts on them and update their status (e.g. in_progress, done) when completed.\n\n",
            marker_suffix
        ));

        let editable_paths: Vec<String> = self
            .context_files
            .iter()
            .filter(|f| f.access == ContextAccess::Editable)
            .map(|f| f.path.clone())
            .collect();

        // Delegate edit protocol specific instructions
        prompt.push_str(&protocol.system_instructions(&editable_paths));

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

        if let Some(plan) = &self.active_plan {
            if let Some(plan_block) = crate::plan::format_active_plan_context(plan) {
                system_text.push_str("## Active Task Plan Context\n");
                system_text.push_str(&plan_block);
                system_text.push_str("\n\n");
            }
        }

        if let Some(review_block) = crate::review::format_active_review_findings(&self.active_review_findings) {
            system_text.push_str("## Active Review Directives\n");
            system_text.push_str(&review_block);
            system_text.push_str("\n\n");
        }

        if let Some(target_lang) = &self.target_language {
            system_text.push_str("## Language Directive\n");
            system_text.push_str(&format!("- Target Response Language: {} (all conversational explanations, answers, and messages MUST be in {})\n", target_lang, target_lang));
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
    fn test_prompt_assembly_with_active_review_findings() {
        let files = vec![];
        let mut assembly = PromptAssembly::new(None, 1, files, "git", "xml", None, None);
        assembly.active_review_findings = vec![ReviewItem {
            id: 1,
            title: "Fix potential panic on unwrap".to_string(),
            severity: tauqe_protocol::ReviewSeverity::Critical,
            status: tauqe_protocol::ReviewStatus::Todo,
            is_checked: true,
            file_path: Some("crates/server/src/main.rs".to_string()),
            line_range: Some((42, 45)),
            body: "Replace unwrap with match.".to_string(),
        }];
        let proto = XmlEditProtocol;
        let messages = assembly.assemble_chat_messages("Resolve finding", &proto);
        assert!(messages[0].content.contains("<active_review_findings>"));
        assert!(messages[0].content.contains("Fix potential panic on unwrap"));
        assert!(messages[0].content.contains("crates/server/src/main.rs:42-45"));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("абвг"), 1);
    }
}
