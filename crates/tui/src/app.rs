use tauqe_protocol::{ContextState, ModelRef, RepositoryState};

use crate::editor::InputEditor;
use crate::ui::context::ContextViewState;
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;
use crate::ui::review::ReviewViewState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCommand {
    pub key: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionDialogKind {
    Model,
}

#[derive(Debug, Clone)]
pub struct SelectionDialogState {
    pub kind: SelectionDialogKind,
    pub items: Vec<ModelRef>,
    pub selected_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Develop,
    Context,
    History,
    Review,
    Onboarding,
}

#[derive(Debug, Clone)]
pub struct ReviewDialogState {
    pub files_count: usize,
    pub estimated_tokens: usize,
    pub models: Vec<ModelRef>,
    pub model_index: usize,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    Git,
    Config,
    Credentials,
    Ready,
    Gatekeeper,
}

#[derive(Debug, Clone)]
pub struct OnboardingState {
    pub step: OnboardingStep,
    pub has_git: bool,
    pub has_config: bool,
    pub config_path: Option<String>,
    pub default_config_path: String,
    pub has_api_key: bool,
    pub credentials_path: Option<String>,
    pub default_credentials_path: String,
    pub selected_index: usize,
    pub input_buffer: String,
    pub input_active: bool,
    pub show_key: bool,
    pub status_message: Option<String>,
    pub error_message: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::Config,
            has_git: true,
            has_config: false,
            config_path: None,
            default_config_path: "tauqe.toml".to_string(),
            has_api_key: false,
            credentials_path: None,
            default_credentials_path: "~/.config/tauqe/credentials.toml".to_string(),
            selected_index: 0,
            input_buffer: String::new(),
            input_active: false,
            show_key: false,
            status_message: None,
            error_message: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderClickAreas {
    pub develop_tab: (u16, u16),
    pub context_tab: (u16, u16),
    pub history_tab: (u16, u16),
    pub review_tab: (u16, u16),
    pub model_select: (u16, u16),
    pub squash_button: (u16, u16),
    pub help_button: (u16, u16),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FooterClickAreas {
    pub help_button: (u16, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SquashBaseMode {
    #[default]
    Session,
    Upstream,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SquashDialogFocus {
    #[default]
    FileList,
    DiffView,
    MessageEditor,
}

#[derive(Debug, Clone)]
pub struct SquashFileItem {
    pub path: String,
    pub diff: String,
    pub expanded: bool,
}

#[derive(Debug, Clone)]
pub struct SquashDialogState {
    pub loading: bool,
    pub generating_message: bool,
    pub base_mode: SquashBaseMode,
    pub base_ref: String,
    pub session_base: Option<String>,
    pub upstream_base: Option<String>,
    pub custom_input: String,
    pub custom_input_active: bool,
    pub commits: Vec<tauqe_protocol::GitSquashCommitItem>,
    pub diff_stat: String,
    pub files: Vec<SquashFileItem>,
    pub selected_file_index: usize,
    pub diff_scroll: u16,
    pub message_buffer: String,
    pub status_message: Option<String>,
    pub focus: SquashDialogFocus,
}

impl Default for SquashDialogState {
    fn default() -> Self {
        Self {
            loading: true,
            generating_message: false,
            base_mode: SquashBaseMode::Session,
            base_ref: String::new(),
            session_base: None,
            upstream_base: None,
            custom_input: String::new(),
            custom_input_active: false,
            commits: Vec::new(),
            diff_stat: String::new(),
            files: Vec::new(),
            selected_file_index: 0,
            diff_scroll: 0,
            message_buffer: String::new(),
            status_message: Some("Inspecting repository history...".to_string()),
            focus: SquashDialogFocus::FileList,
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
    pub active_model: ModelRef,
    pub available_models: Vec<ModelRef>,
    pub available_workflows: Vec<String>,
    pub available_edit_protocols: Vec<String>,
    pub model: DevelopView,
    pub context: ContextState,
    pub context_view: ContextViewState,
    pub history_view: HistoryViewState,
    pub review: ReviewViewState,
    pub onboarding: OnboardingState,
    pub input_editor: InputEditor,
    pub show_help: bool,
    pub confirm_cancel: bool,
    pub confirm_undo: bool,
    pub confirm_clear_history: bool,
    pub selection_dialog: Option<SelectionDialogState>,
    pub squash_dialog: Option<SquashDialogState>,
    pub review_dialog: Option<ReviewDialogState>,
    pub last_model_height: u16,
    pub header_clicks: HeaderClickAreas,
    pub footer_clicks: FooterClickAreas,
}

impl AppState {
    pub fn take_prompt(&mut self) -> Option<String> {
        if self.input_editor.is_empty() {
            return None;
        }
        if self.model.is_busy() || self.review.running {
            self.model.git_notification = Some(
                "Model is generating. Press Esc to cancel or wait until done.".to_string(),
            );
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
        self.model.error = None;
        self.model.result = None;
        self.model.usage = None;
        self.model.scroll = 0;
        self.model.status = "awaiting".to_string();
        self.model.auto_scroll = true;
        self.model.current_cost = Some(0.0);

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
        self.context_view
            .update_filtered_candidates(&self.context.items, &self.all_repo_files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_prompt_when_busy_preserves_input() {
        let mut state = AppState {
            view_mode: ViewMode::Develop,
            protocol_version: "1.0".to_string(),
            repo_state: None,
            all_repo_files: Vec::new(),
            workflow: "git".to_string(),
            edit_protocol: "xml".to_string(),
            active_model: ModelRef::openrouter("test-model"),
            available_models: Vec::new(),
            available_workflows: Vec::new(),
            available_edit_protocols: Vec::new(),
            model: DevelopView {
                status: "thinking".to_string(),
                ..Default::default()
            },
            context: ContextState::default(),
            context_view: ContextViewState::default(),
            history_view: HistoryViewState::default(),
            review: ReviewViewState::default(),
            review_dialog: None,
            onboarding: OnboardingState::default(),
            input_editor: InputEditor::default(),
            show_help: false,
            confirm_cancel: false,
            confirm_undo: false,
            confirm_clear_history: false,
            selection_dialog: None,
            squash_dialog: None,
            last_model_height: 10,
            header_clicks: HeaderClickAreas::default(),
            footer_clicks: FooterClickAreas::default(),
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
