use crate::context::ContextFileContent;
use crate::model::gateway::ChatMessage;
use workbench_protocol::{ContextAccess, RepositoryState};

pub struct PromptAssembly {
    pub repo_state: Option<RepositoryState>,
    pub context_revision: u64,
    pub context_files: Vec<ContextFileContent>,
}

impl PromptAssembly {
    pub fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
    ) -> Self {
        Self {
            repo_state,
            context_revision,
            context_files,
        }
    }

    pub fn build_system_prompt(&self) -> String {
        let mut prompt = String::new();
        prompt.push_str("You are Workbench AI, an expert programming assistant operating inside the Workbench development environment.\n\n");
        prompt.push_str("## Core System Contract\n");
        prompt.push_str("1. Authoritative Source: The files provided in <context> represent the authoritative current state of the project. Do not invent missing code.\n");
        prompt.push_str("2. Read-Only Scope: Files inside <read_only_files> are strictly for reference and understanding. Do NOT propose edits to them.\n");
        prompt.push_str("3. Editable Scope: Only files inside <editable_files> are permitted for modification.\n");
        prompt.push_str("4. No Arbitrary Shell: You do not have shell execution capabilities. Work strictly through the context and actions provided.\n");
        prompt.push_str("5. Minimal Coherent Change: Prefer the smallest coherent modification necessary to complete the task.\n\n");

        if let Some(repo) = &self.repo_state {
            prompt.push_str("## Project Metadata\n");
            prompt.push_str(&format!("- Root: {}\n", repo.root));
            prompt.push_str(&format!("- Branch: {}\n", repo.branch));
            prompt.push_str(&format!("- HEAD: {}\n", repo.head));
            let dirty_str = if repo.dirty {
                "dirty (uncommitted changes present)"
            } else {
                "clean"
            };
            prompt.push_str(&format!("- Status: {}\n\n", dirty_str));
        }

        prompt
    }

    pub fn format_context_block(&self) -> Option<String> {
        if self.context_files.is_empty() {
            return None;
        }

        let mut out = String::new();
        out.push_str(&format!("<context revision=\"{}\">\n", self.context_revision));

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

    pub fn assemble_chat_messages(
        &self,
        conversation_history: &[ChatMessage],
        current_prompt: &str,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        // Layer 1-3: System prompt + Project metadata + Authoritative context
        let mut system_text = self.build_system_prompt();
        if let Some(context_block) = self.format_context_block() {
            system_text.push_str("## Project Context\n");
            system_text.push_str(&context_block);
            system_text.push_str("\n\n");
        }

        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system_text,
        });

        // Layer 5: Conversation history
        for msg in conversation_history {
            messages.push(msg.clone());
        }

        // Layer 10: Current user request
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: current_prompt.to_string(),
        });

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_block_formatting() {
        let files = vec![
            ContextFileContent {
                path: "docs/Core.md".to_string(),
                access: ContextAccess::ReadOnly,
                content: "# Core Architecture\n".to_string(),
            },
            ContextFileContent {
                path: "src/main.rs".to_string(),
                access: ContextAccess::Editable,
                content: "fn main() {}\n".to_string(),
            },
        ];

        let assembly = PromptAssembly::new(None, 42, files);
        let block = assembly.format_context_block().unwrap();

        assert!(block.contains("<context revision=\"42\">"));
        assert!(block.contains("<read_only_files>"));
        assert!(block.contains("<file path=\"docs/Core.md\">"));
        assert!(block.contains("<editable_files>"));
        assert!(block.contains("<file path=\"src/main.rs\">"));
    }

    #[test]
    fn test_assemble_chat_messages() {
        let assembly = PromptAssembly::new(None, 1, Vec::new());
        let messages = assembly.assemble_chat_messages(&[], "Hello!");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "Hello!");
    }
}
