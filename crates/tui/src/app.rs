use tauqe_protocol::{matches_glob_pattern, ContextState, RepositoryState, UiHistoryItem};

use crate::context_view::ContextViewState;
use crate::editor::InputEditor;
use crate::model_view::ModelView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionDialogKind {
    Workflow,
    EditProtocol,
    Model,
}

#[derive(Debug, Clone)]
pub struct SelectionDialogState {
    pub kind: SelectionDialogKind,
    pub items: Vec<String>,
    pub selected_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Model,
    Context,
    History,
    Onboarding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Config,
    Credentials,
    Ready,
    Gatekeeper,
}

#[derive(Debug, Clone)]
pub struct OnboardingState {
    pub step: OnboardingStep,
    pub has_config: bool,
    pub config_path: Option<String>,
    pub default_config_path: String,
    pub has_api_key: bool,
    pub credentials_path: Option<String>,
    pub default_credentials_path: String,
    pub selected_index: usize,
    pub input_buffer: String,
    pub input_active: bool,
    pub status_message: Option<String>,
    pub error_message: Option<String>,
    pub selected_model: String,
    pub models_list: Vec<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::Config,
            has_config: false,
            config_path: None,
            default_config_path: "tauqe.toml".to_string(),
            has_api_key: false,
            credentials_path: None,
            default_credentials_path: "~/.config/tauqe/credentials.toml".to_string(),
            selected_index: 0,
            input_buffer: String::new(),
            input_active: false,
            status_message: None,
            error_message: None,
            selected_model: "anthropic/claude-3.7-sonnet".to_string(),
            models_list: vec![
                "anthropic/claude-3.7-sonnet".to_string(),
                "anthropic/claude-3.5-sonnet".to_string(),
                "openai/gpt-4o".to_string(),
                "deepseek/deepseek-chat".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub struct HistoryViewState {
    pub items: Vec<UiHistoryItem>,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub has_more: bool,
    pub total_count: usize,
    pub loading: bool,
    pub rendered_lines_count: usize,
    pub pending_before_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SquashDialogState {
    pub loading: bool,
    pub base_ref: String,
    pub commits: Vec<tauqe_protocol::GitSquashCommitItem>,
    pub diff_stat: String,
    pub message_buffer: String,
    pub status_message: Option<String>,
}

impl Default for HistoryViewState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            scroll: 0,
            auto_scroll: true,
            has_more: false,
            total_count: 0,
            loading: false,
            rendered_lines_count: 0,
            pending_before_id: None,
        }
    }
}

pub struct AppState {
    pub view_mode: ViewMode,
    pub protocol_version: String,
    pub repo_state: Option<RepositoryState>,
    pub all_repo_files: Vec<String>,
    pub workflow: String,
    pub edit_protocol: String,
    pub active_model: String,
    pub available_models: Vec<String>,
    pub available_workflows: Vec<String>,
    pub available_edit_protocols: Vec<String>,
    pub model: ModelView,
    pub context: ContextState,
    pub context_view: ContextViewState,
    pub history_view: HistoryViewState,
    pub onboarding: OnboardingState,
    pub input_editor: InputEditor,
    pub show_help: bool,
    pub confirm_cancel: bool,
    pub confirm_undo: bool,
    pub confirm_clear_history: bool,
    pub selection_dialog: Option<SelectionDialogState>,
    pub squash_dialog: Option<SquashDialogState>,
    pub last_model_height: u16,
}

impl AppState {
    pub fn take_prompt(&mut self) -> Option<String> {
        if self.input_editor.is_empty() {
            return None;
        }
        if self.model.is_busy() {
            self.model.git_notification = Some(
                "Model is generating. Press Esc to cancel or wait until done.".to_string(),
            );
            if let Some(file) = self.model.files.get_mut(self.model.selected_file_index) {
                file.expanded = !file.expanded;
            }
            return None;
        }
        let prompt = self.input_editor.get_text().trim().to_string();
        self.input_editor.clear();
        self.confirm_clear_history = false;
        self.confirm_cancel = false;
        if prompt.is_empty() {
            return None;
        }

        self.model.reasoning.clear();
        self.model.text.clear();
        self.model.markdown_lines.clear();
        self.model.reasoning_lines.clear();
        self.model.error = None;
        self.model.result = None;
        self.model.usage = None;
        self.model.scroll = 0;
        self.model.status = "awaiting".to_string();
        self.model.show_reasoning = true;
        self.model.auto_scroll = true;
        self.model.current_cost = Some(0.0);
        self.model.round_usage_received = false;

        self.model.edits_active = false;
        self.model.files.clear();
        self.model.selected_file_index = 0;
        self.model.edit_final_applied = None;
        self.model.edit_final_error = None;
        self.model.last_commit_hash = None;
        self.model.last_commit_summary = None;
        self.model.git_notification = None;
        self.model.toolchain_command = None;
        self.model.toolchain_status = None;
        self.model.copy_flash = None;
        self.model.copy_notification = None;
        self.model.code_blocks.clear();

        Some(prompt)
    }

    pub fn update_filtered_candidates(&mut self) {
        let query = self.context_view.add_input.trim();
        let existing: std::collections::HashSet<&str> = self
            .context
            .items
            .iter()
            .map(|it| it.path.as_str())
            .collect();

        let mut candidates = Vec::new();

        if !query.is_empty() {
            let is_pattern_query = query.contains('*')
                || query.contains('?')
                || query.ends_with('/')
                || !query.contains('.');

            let matching_pattern_count = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()) && matches_glob_pattern(query, f))
                .count();

            if matching_pattern_count > 0 && is_pattern_query {
                candidates.push(format!(
                    "[+] Add all matching '{}' ({} files)",
                    query, matching_pattern_count
                ));
            }

            let query_lower = query.to_lowercase();
            let file_candidates: Vec<String> = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .filter(|f| {
                    f.to_lowercase().contains(&query_lower) || matches_glob_pattern(query, f)
                })
                .take(15)
                .cloned()
                .collect();

            candidates.extend(file_candidates);
        } else {
            candidates = self
                .all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .take(15)
                .cloned()
                .collect();
        }

        self.context_view.filtered_candidates = candidates;

        if self.context_view.filtered_candidates.is_empty() {
            self.context_view.selected_candidate_index = 0;
        } else if self.context_view.selected_candidate_index
            >= self.context_view.filtered_candidates.len()
        {
            self.context_view.selected_candidate_index =
                self.context_view.filtered_candidates.len() - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_prompt_when_busy_preserves_input() {
        let mut state = AppState {
            view_mode: ViewMode::Model,
            protocol_version: "1.0".to_string(),
            repo_state: None,
            all_repo_files: Vec::new(),
            workflow: "git".to_string(),
            edit_protocol: "xml".to_string(),
            active_model: "test-model".to_string(),
            available_models: Vec::new(),
            available_workflows: Vec::new(),
            available_edit_protocols: Vec::new(),
            model: ModelView {
                status: "thinking".to_string(),
                ..Default::default()
            },
            context: ContextState::default(),
            context_view: ContextViewState::default(),
            history_view: HistoryViewState::default(),
            onboarding: OnboardingState::default(),
            input_editor: InputEditor::default(),
            show_help: false,
            confirm_cancel: false,
            confirm_undo: false,
            confirm_clear_history: false,
            selection_dialog: None,
            squash_dialog: None,
            last_model_height: 10,
        };

        state.input_editor.insert_str("next planned prompt");
        assert!(state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, None);
        assert_eq!(state.input_editor.get_text(), "next planned prompt");
        assert!(state.model.git_notification.is_some());

        state.model.status = "done".to_string();
        assert!(!state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, Some("next planned prompt".to_string()));
        assert!(state.input_editor.is_empty());
    }
}
