use std::path::PathBuf;
use crate::context::ContextFileContent;
use crate::edits::EditProtocol;
use crate::model::gateway::{ChatMessage, FunctionDefinition, ToolDefinition};
use workbench_protocol::{ContextAccess, IntentState, RepositoryState};

#[derive(Debug, Clone)]
pub struct IntentManager {
    repo_root: PathBuf,
    file_path: String,
    max_tokens: usize,
    content: String,
}

impl IntentManager {
    pub fn new(repo_root: PathBuf, file_path: String, max_tokens: usize) -> Self {
        let mut mgr = Self {
            repo_root,
            file_path,
            max_tokens,
            content: String::new(),
        };
        let _ = mgr.load();
        mgr
    }

    pub fn set_repo_root(&mut self, repo_root: PathBuf) {
        self.repo_root = repo_root;
        let _ = self.load();
    }

    pub fn full_path(&self) -> PathBuf {
        self.repo_root.join(&self.file_path)
    }

    pub fn load(&mut self) -> std::io::Result<()> {
        let path = self.full_path();
        if path.is_file() {
            let content = std::fs::read_to_string(path)?;
            self.content = content;
        } else {
            self.content.clear();
        }
        Ok(())
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = self.full_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &self.content)?;
        Ok(())
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn set_content(&mut self, content: String) -> std::io::Result<()> {
        self.content = content;
        self.save()
    }

    pub fn clear(&mut self) -> std::io::Result<()> {
        self.content.clear();
        let path = self.full_path();
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.content)
    }

    pub fn get_state(&self) -> IntentState {
        IntentState {
            content: self.content.clone(),
            estimated_tokens: self.estimated_tokens() as u64,
            max_tokens: self.max_tokens,
            file_path: self.file_path.clone(),
        }
    }

    pub fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "update_intent_memory".to_string(),
                description: "Update the persistent task intent memory. This is your concise internal notepad to resume context across turns. Record: (1) Current problem/topic under discussion, (2) Brief labels of proposed options on the table so you can recognize them if the user picks 'option 1', (3) Confirmed decisions ONLY if explicitly approved by the user, and (4) What input is currently awaited from the user. Keep it ultra-compact (5-15 lines). Never duplicate project docs or system rules.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Ultra-compact markdown note to yourself: ### Current Focus, ### Options On Table (brief tags), ### Confirmed Decisions (only user-approved), ### Awaiting."
                        },
                        "explanation": {
                            "type": "string",
                            "description": "Short note on what conversation state changed."
                        }
                    },
                    "required": ["content"]
                }),
            },
        }
    }
}

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
    pub intent_memory: Option<String>,
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
            intent_memory: None,
        }
    }

    pub fn with_intent_memory(mut self, intent: Option<String>) -> Self {
        self.intent_memory = intent;
        self
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
        prompt.push_str("6. Always Explain Changes: Whenever you propose file edits (via XML or tool calls), you MUST precede them with a concise conversational explanation (1-3 sentences) explaining what changes were made, why, and how they achieve the user's intent. Never output edits alone without an accompanying explanation.\n\n");

        if let Some(intent) = &self.intent_memory {
            if !intent.trim().is_empty() {
                prompt.push_str("## Persistent Intent Memory (Internal Task Notepad)\n");
                prompt.push_str("This is your compact working memory from previous turns to maintain continuity without keeping raw conversation logs:\n");
                prompt.push_str("<intent_memory>\n");
                prompt.push_str(intent.trim());
                prompt.push_str("\n</intent_memory>\n\n");
            }
        }

        prompt.push_str("## Intent Memory & Conversational Continuity\n");
        prompt.push_str("You have the tool `update_intent_memory`. It serves as an action-oriented state summary for continuous understanding:\n");
        prompt.push_str("1. ALWAYS PROVIDE A DIRECT TEXT ANSWER to the user. Never return an empty message or only tool calls when answering the user. If the user asks a question, give a comprehensive, direct answer.\n");
        prompt.push_str("2. Compact note to yourself: In `update_intent_memory`, write a high-density note (5-15 lines) so that if the user replies with 'let's do option 2' or an abrupt follow-up, you will immediately understand what is meant.\n");
        prompt.push_str("3. Structure:\n");
        prompt.push_str("   ### Current Focus: (what problem or topic is being discussed)\n");
        prompt.push_str("   ### Options On Table: (1-2 line labels of the options you proposed to the user)\n");
        prompt.push_str("   ### Confirmed Decisions: (ONLY include decisions that the user has explicitly accepted. Do NOT invent decisions during exploration!)\n");
        prompt.push_str("   ### Awaiting: (what user input or confirmation is awaited)\n");
        prompt.push_str("4. Anti-duplication: Do NOT repeat architecture, coding conventions, or rules already present in project files or this prompt.\n\n");

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

        // Layer 1-3: System prompt + Intent memory + Project metadata + Authoritative context + Protocol instructions
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
    fn test_intent_manager_and_prompt_assembly() {
        let temp_dir = std::env::temp_dir().join("wb_test_intent");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut intent_mgr = IntentManager::new(temp_dir.clone(), ".workbench/intent.md".to_string(), 1000);
        assert_eq!(intent_mgr.content(), "");

        intent_mgr.set_content("### Current Focus\nTUI performance\n### Confirmed Decisions\nAvoid extra allocations".to_string()).unwrap();
        assert!(intent_mgr.estimated_tokens() > 0);

        let files = vec![ContextFileContent {
            path: "src/main.rs".to_string(),
            access: ContextAccess::Editable,
            content: "fn main() {}\n".to_string(),
        }];

        let assembly = PromptAssembly::new(None, 1, files, "toolchain", "xml")
            .with_intent_memory(Some(intent_mgr.content().to_string()));

        let proto = XmlEditProtocol::default();
        let prompt_str = assembly.build_system_prompt(&proto);
        assert!(prompt_str.contains("## Persistent Intent Memory (Internal Task Notepad)"));
        assert!(prompt_str.contains("TUI performance"));

        let _ = intent_mgr.clear();
        assert_eq!(intent_mgr.content(), "");
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
