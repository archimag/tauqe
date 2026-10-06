use workbench_protocol::ContextAccess;

#[derive(Debug, Clone)]
pub struct ContextViewState {
    pub cursor_index: usize,
    pub adding_file: bool,
    pub add_access: ContextAccess,
    pub add_input: String,
    pub filtered_candidates: Vec<String>,
    pub selected_candidate_index: usize,
    pub status_message: Option<String>,
}

impl Default for ContextViewState {
    fn default() -> Self {
        Self {
            cursor_index: 0,
            adding_file: false,
            add_access: ContextAccess::ReadOnly,
            add_input: String::new(),
            filtered_candidates: Vec::new(),
            selected_candidate_index: 0,
            status_message: None,
        }
    }
}
