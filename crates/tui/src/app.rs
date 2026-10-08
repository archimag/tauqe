use tauqe_protocol::{ContextState, ModelRef, RepositoryState};

use crate::config::TuiConfig;
use crate::editor::InputEditor;
use crate::ui::context::ContextViewState;
use crate::ui::develop::DevelopView;
use crate::ui::history::HistoryViewState;
use crate::ui::plans::PlansViewState;
use crate::ui::review::ReviewViewState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCommand {
    pub key: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmDialogButton {
    #[default]
    Cancel,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct AppNotification {
    pub level: NotificationLevel,
    pub text: String,
    pub created_at: std::time::Instant,
    pub ttl: std::time::Duration,
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
    Review,
    Plans,
    History,
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
    Workstation,
    Modifier,
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
    pub has_tui_config: bool,
    pub default_tui_config_path: String,
    pub chosen_layout: crate::config::LayoutPreset,
    pub chosen_langmap: Option<String>,
    pub selected_index: usize,
    pub input_buffer: String,
    pub input_active: bool,
    pub input_langmap: bool,
    pub show_key: bool,
    pub status_message: Option<String>,
    pub error_message: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        let has_tui = crate::config::find_tui_config_path().is_some();
        let default_tui = crate::config::default_tui_config_path().display().to_string();
        Self {
            step: OnboardingStep::Config,
            has_git: true,
            has_config: false,
            config_path: None,
            default_config_path: "tauqe.toml".to_string(),
            has_api_key: false,
            credentials_path: None,
            default_credentials_path: "~/.config/tauqe/credentials.toml".to_string(),
            has_tui_config: has_tui,
            default_tui_config_path: default_tui,
            chosen_layout: crate::config::LayoutPreset::None,
            chosen_langmap: None,
            selected_index: 0,
            input_buffer: String::new(),
            input_active: false,
            input_langmap: false,
            show_key: false,
            status_message: None,
            error_message: None,
        }
    }
}

impl OnboardingState {
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
        self.error_message = None;
    }

    pub fn set_error(&mut self, err: impl Into<String>) {
        self.error_message = Some(err.into());
        self.status_message = None;
    }

    pub fn clear_messages(&mut self) {
        self.status_message = None;
        self.error_message = None;
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderClickAreas {
    pub row: u16,
    pub develop_tab: (u16, u16),
    pub context_tab: (u16, u16),
    pub review_tab: (u16, u16),
    pub plans_tab: (u16, u16),
    pub history_tab: (u16, u16),
    pub model_select: (u16, u16),
    pub squash_button: (u16, u16),
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
    pub pending_base: Option<(SquashBaseMode, String)>,
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
    pub confirm_apply: bool,
    pub confirm_button: ConfirmDialogButton,
}

impl Default for SquashDialogState {
    fn default() -> Self {
        Self {
            loading: true,
            generating_message: false,
            base_mode: SquashBaseMode::Session,
            base_ref: String::new(),
            pending_base: None,
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
            confirm_apply: false,
            confirm_button: ConfirmDialogButton::Cancel,
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
    pub plans_view: PlansViewState,
    pub onboarding: OnboardingState,
    pub input_editor: InputEditor,
    pub tui_config: TuiConfig,
    pub show_help: bool,
    pub help_scroll: u16,
    pub confirm_cancel: bool,
    pub confirm_quit: bool,
    pub confirm_undo: bool,
    pub confirm_clear_history: bool,
    pub confirm_delete_plan: Option<String>,
    pub confirm_button: ConfirmDialogButton,
    pub selection_dialog: Option<SelectionDialogState>,
    pub squash_dialog: Option<SquashDialogState>,
    pub review_dialog: Option<ReviewDialogState>,
    pub notification: Option<AppNotification>,
    pub turn_started_at: Option<std::time::Instant>,
    pub server_disconnected: Option<String>,
    pub server_log_path: std::path::PathBuf,
    pub last_model_height: u16,
    pub header_clicks: HeaderClickAreas,
}

impl AppState {
    pub fn notify(&mut self, text: impl Into<String>, level: NotificationLevel, ttl: std::time::Duration) {
        self.notification = Some(AppNotification {
            level,
            text: text.into(),
            created_at: std::time::Instant::now(),
            ttl,
        });
    }

    pub fn notify_info(&mut self, text: impl Into<String>) {
        self.notify(text, NotificationLevel::Info, std::time::Duration::from_secs(3));
    }

    pub fn notify_success(&mut self, text: impl Into<String>) {
        self.notify(text, NotificationLevel::Success, std::time::Duration::from_secs(4));
    }

    pub fn notify_warning(&mut self, text: impl Into<String>) {
        self.notify(text, NotificationLevel::Warning, std::time::Duration::from_secs(4));
    }

    pub fn notify_error(&mut self, text: impl Into<String>) {
        self.notify(text, NotificationLevel::Error, std::time::Duration::from_secs(6));
    }

    pub fn has_active_modal(&self) -> bool {
        self.server_disconnected.is_some()
            || self.show_help
            || self.selection_dialog.is_some()
            || self.squash_dialog.is_some()
            || self.review_dialog.is_some()
            || self.confirm_cancel
            || self.confirm_quit
            || self.confirm_undo
            || self.confirm_clear_history
            || self.confirm_delete_plan.is_some()
            || self.context_view.confirm_clear_auto
            || self.context_view.adding_file
    }

    pub fn active_notification(&self) -> Option<&AppNotification> {
        if let Some(ref notif) = self.notification {
            if notif.created_at.elapsed() < notif.ttl {
                return Some(notif);
            }
        }
        None
    }

    pub fn take_prompt(&mut self) -> Option<String> {
        if self.input_editor.is_empty() {
            return None;
        }
        if self.model.is_busy() || self.review.running {
            self.notify_warning("Model is generating. Press Esc to cancel or wait until done.");
            return None;
        }
        let prompt = self.input_editor.get_text().trim().to_string();
        self.input_editor.record_history(&prompt);
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
        self.model.toolchain_command = None;
        self.model.toolchain_status = None;
        self.model.copy_flash = None;
        self.model.code_blocks.clear();
        self.turn_started_at = Some(std::time::Instant::now());

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
            plans_view: PlansViewState::default(),
            review_dialog: None,
            notification: None,
            turn_started_at: None,
            onboarding: OnboardingState::default(),
            input_editor: InputEditor::default(),
            tui_config: TuiConfig::default(),
            show_help: false,
            help_scroll: 0,
            confirm_cancel: false,
            confirm_quit: false,
            confirm_undo: false,
            confirm_clear_history: false,
            confirm_delete_plan: None,
            confirm_button: ConfirmDialogButton::Cancel,
            selection_dialog: None,
            squash_dialog: None,
            server_disconnected: None,
            server_log_path: std::path::PathBuf::from(".tauqe/server.log"),
            last_model_height: 10,
            header_clicks: HeaderClickAreas::default(),
        };

        state.input_editor.insert_str("next planned prompt");
        assert!(state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, None);
        assert_eq!(state.input_editor.get_text(), "next planned prompt");
        assert!(state.active_notification().is_some());

        state.model.status = "done".to_string();
        assert!(!state.model.is_busy());

        let result = state.take_prompt();
        assert_eq!(result, Some("next planned prompt".to_string()));
        assert!(state.input_editor.is_empty());
    }
}
