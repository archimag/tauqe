use crate::context::ContextFileContent;
use crate::edits::EditProtocol;
use crate::model::gateway::ChatMessage;
use workbench_protocol::{ContextAccess, RepositoryState};

pub struct PromptAssembly {
    pub repo_state: Option<RepositoryState>,
    pub context_revision: u64,
    pub context_files: Vec<ContextFileContent>,
    pub workflow: String,
    pub edit_protocol: String,
}

impl PromptAssembly {
    pub fn new(
        repo_state: Option<RepositoryState>,
        context_revision: u64,
        context_files: Vec<ContextFileContent>,
        workflow: impl Into<String>,
        edit_protocol: impl Into<String>,
    ) -> Self {
        Self {
            repo_state,
            context_revision,
            context_files,
            workflow: workflow.into(),
            edit_protocol: edit_protocol.into(),
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
        prompt.push_str("5. Minimal Coherent Change: Prefer the smallest coherent modification necessary to complete the task.\n\n");

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
        protocol: &dyn EditProtocol,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        // Layer 1-3: System prompt + Project metadata + Authoritative context + Protocol instructions
        let mut system_text = self.build_system_prompt(protocol);
        if let Some(context_block) = self.format_context_block() {
            system_text.push_str("## Project Context\n");
            system_text.push_str(&context_block);
            system_text.push_str("\n\n");
        }

        messages.push(ChatMessage::system(system_text));

        // Layer 5: Conversation history
        for msg in conversation_history {
            messages.push(msg.clone());
        }

        // Layer 10: Current user request
        messages.push(ChatMessage::user(current_prompt));

        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edits::XmlEditProtocol;

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

        let assembly = PromptAssembly::new(None, 42, files, "toolchain", "xml");
        let block = assembly.format_context_block().unwrap();

        assert!(block.contains("<context revision=\"42\">"));
        assert!(block.contains("<read_only_files>"));
        assert!(block.contains("<file path=\"docs/Core.md\">"));
        assert!(block.contains("<editable_files>"));
        assert!(block.contains("<file path=\"src/main.rs\">"));
    }

    #[test]
    fn test_assemble_chat_messages() {
        let assembly = PromptAssembly::new(None, 1, Vec::new(), "toolchain", "xml");
        let proto = XmlEditProtocol::default();
        let messages = assembly.assemble_chat_messages(&[], "Hello!", &proto);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "Hello!");
        assert!(messages[0].content.contains("- Workflow: toolchain"));
        assert!(messages[0].content.contains("- Edit Protocol: xml"));
    }
}
