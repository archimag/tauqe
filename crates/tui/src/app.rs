use workbench_protocol::{matches_glob_pattern, ContextState, RepositoryState};

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
    pub input_editor: InputEditor,
    pub show_help: bool,
    pub confirm_undo: bool,
    pub selection_dialog: Option<SelectionDialogState>,
    pub last_model_height: u16,
}

impl AppState {
    pub fn take_prompt(&mut self) -> Option<String> {
        if self.input_editor.is_empty() {
            return None;
        }
        let prompt = self.input_editor.get_text().trim().to_string();
        self.input_editor.clear();
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
        self.model.status = "starting".to_string();
        self.model.show_reasoning = true;
        self.model.auto_scroll = true;

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
        } else if self.context_view.selected_candidate_index >= self.context_view.filtered_candidates.len() {
            self.context_view.selected_candidate_index =
                self.context_view.filtered_candidates.len() - 1;
        }
    }
}
