use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRow {
    Header(ContextLayer),
    Item(ContextItem),
}

#[derive(Debug, Clone)]
pub struct ContextViewState {
    pub cursor_index: usize,
    pub pinned_expanded: bool,
    pub user_expanded: bool,
    pub auto_expanded: bool,
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
            pinned_expanded: true,
            user_expanded: true,
            auto_expanded: true,
            adding_file: false,
            add_access: ContextAccess::ReadOnly,
            add_input: String::new(),
            filtered_candidates: Vec::new(),
            selected_candidate_index: 0,
            status_message: None,
        }
    }
}

impl ContextViewState {
    pub fn compute_rows(&self, items: &[ContextItem]) -> Vec<ContextRow> {
        let mut rows = Vec::new();

        rows.push(ContextRow::Header(ContextLayer::Pinned));
        if self.pinned_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::Pinned) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows.push(ContextRow::Header(ContextLayer::User));
        if self.user_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::User) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows.push(ContextRow::Header(ContextLayer::Auto));
        if self.auto_expanded {
            for it in items.iter().filter(|i| i.layer == ContextLayer::Auto) {
                rows.push(ContextRow::Item(it.clone()));
            }
        }

        rows
    }

    pub fn toggle_section(&mut self, layer: ContextLayer) {
        match layer {
            ContextLayer::Pinned => self.pinned_expanded = !self.pinned_expanded,
            ContextLayer::User => self.user_expanded = !self.user_expanded,
            ContextLayer::Auto => self.auto_expanded = !self.auto_expanded,
        }
    }
}
