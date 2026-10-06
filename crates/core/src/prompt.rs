use crate::context::ContextFileContent;
use crate::edits::EditProtocol;
use crate::model::gateway::ChatMessage;
use workbench_protocol::{ContextAccess, RepositoryState};

pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    // Heuristic: ~4 characters per token
    (text.chars().count() + 3) / 4
}

pub struct PromptAssembly {
    pub repo_state: Option<RepositoryState>,
    pub context_revision: u64,
    pub context_files: Vec<ContextFileContent>,
    pub workflow: String,
    pub edit_protocol: String,
    pub history_tag: Option<String>,
}

impl PromptAssembly {
    pub fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
        workflow: impl Into<String>,
        edit_protocol: impl Into<String>,
        history_tag: Option<String>,
    ) -> Self {
        Self {
            repo_state,
            context_revision,
            context_files,
            workflow: workflow.into(),
            edit_protocol: edit_protocol.into(),
            history_tag,
        }
    }

    pub fn build_system_prompt(&self, protocol: &dyn EditProtocol) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are Workbench AI, an expert programming assistant operating inside the Workbench development environment.\n\n");
        prompt.push_str("## Core System Contract\n");
        prompt.push_str("1. Authoritative Source: The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.\n");
        prompt.push_str("2. Read-Only Scope: Files inside <read_only_files> are strictly for reference and understanding. Do NOT propose edits to them.\n");
        prompt.push_str("3. Editable Scope: Files inside <editable_files> are permitted for modification. You may also create new files using <create path=\"...\"> when required by the task.\n");
        prompt.push_str("4. No Arbitrary Shell: You do not have shell execution capabilities. Work strictly through the context and actions provided.\n");
        prompt.push_str("5. Minimal Coherent Change: Prefer the smallest coherent modification necessary to complete the task.\n");
        prompt.push_str("6. Always Explain Changes: Whenever you propose file edits, you MUST precede them with a concise conversational explanation (1-3 sentences) explaining what changes were made, why, and how they achieve the user's intent. Never output edits alone without an accompanying explanation.\n\n");

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

    pub fn assemble_chat_messages(
        &self,
        current_prompt: &str,
        protocol: &dyn EditProtocol,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        // System prompt + Project metadata + Authoritative context + Session history + Protocol instructions
        let mut system_text = self.build_system_prompt(protocol);
        if let Some(context_block) = self.format_context_block() {
            system_text.push_str("## Project Context\n");
            system_text.push_str(&context_block);
            system_text.push_str("\n\n");
        }

        if let Some(history_block) = self.format_history_block() {
            system_text.push_str("## Session History\n");
            system_text.push_str(&history_block);
            system_text.push_str("\n\n");
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
    fn test_prompt_assembly() {
        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];
        let history = Some("<history>\n{\"type\":\"summary\",\"text\":\"Initial\"}\n</history>".to_string());
        let assembly = PromptAssembly::new(None, 1, files, "toolchain", "xml", history);
        let proto = XmlEditProtocol::default();
        let messages = assembly.assemble_chat_messages("Explain this code", &proto);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("Core System Contract"));
        assert!(messages[0].content.contains("src/main.rs"));
        assert!(messages[0].content.contains("fn main() {}"));
        assert!(messages[0].content.contains("## Session History"));
        assert!(messages[0].content.contains("<history>"));
        assert!(messages[0].content.contains("{\"type\":\"summary\",\"text\":\"Initial\"}"));
        assert_eq!(messages[1], ChatMessage::user("Explain this code"));
    }

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("абвг"), 1);
    }
}
