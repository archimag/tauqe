use tauqe_protocol::{matches_glob_pattern, ContextAccess, ContextItem, ContextLayer};

use crate::editor::InputEditor;

pub mod input;
pub mod render;

pub use self::input::{handle_context_key, CONTEXT_COMMANDS};
pub use self::render::render_context_view;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRow {
    Header(ContextLayer),
    Item(ContextItem),
}

#[derive(Debug, Clone)]
pub struct ContextViewState {
    pub cursor_index: usize,
    pub scroll: u16,
    pub pinned_expanded: bool,
    pub user_expanded: bool,
    pub auto_expanded: bool,
    pub adding_file: bool,
    pub add_access: ContextAccess,
    pub add_input: InputEditor,
    pub filtered_candidates: Vec<String>,
    pub total_matching: usize,
    pub selected_candidate_index: usize,
    pub confirm_clear_auto: bool,
}

impl Default for ContextViewState {
    fn default() -> Self {
        Self {
            cursor_index: 0,
            scroll: 0,
            pinned_expanded: true,
            user_expanded: true,
            auto_expanded: true,
            adding_file: false,
            add_access: ContextAccess::ReadOnly,
            add_input: InputEditor::default(),
            filtered_candidates: Vec::new(),
            total_matching: 0,
            selected_candidate_index: 0,
            confirm_clear_auto: false,
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

    pub fn update_filtered_candidates(
        &mut self,
        items: &[ContextItem],
        all_repo_files: &[String],
    ) {
        let query = self.add_input.get_text().trim();
        let existing: std::collections::HashSet<&str> =
            items.iter().map(|it| it.path.as_str()).collect();

        let mut candidates = Vec::new();

        if !query.is_empty() {
            let is_pattern_query = query.contains('*')
                || query.contains('?')
                || query.ends_with('/')
                || !query.contains('.');

            let matching_pattern_count = all_repo_files
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
            let all_matches: Vec<String> = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .filter(|f| {
                    f.to_lowercase().contains(&query_lower) || matches_glob_pattern(query, f)
                })
                .cloned()
                .collect();

            self.total_matching = all_matches.len();
            candidates.extend(all_matches.into_iter().take(50));
        } else {
            let available: Vec<String> = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .cloned()
                .collect();
            self.total_matching = available.len();
            candidates = available.into_iter().take(50).collect();
        }

        self.filtered_candidates = candidates;

        // The synthetic "[+] Add all matching" row is convenient but risky: a
        // single Enter mass-adds every match and can bloat the context without
        // warning. Default the selection to the first concrete file so Enter is
        // safe, while keeping the bulk row reachable by navigating up.
        let bulk_row_first = self
            .filtered_candidates
            .first()
            .is_some_and(|c| c.starts_with("[+] Add all matching '"));

        if self.filtered_candidates.is_empty() {
            self.selected_candidate_index = 0;
        } else if self.selected_candidate_index == 0
            && bulk_row_first
            && self.filtered_candidates.len() > 1
        {
            self.selected_candidate_index = 1;
        } else if self.selected_candidate_index >= self.filtered_candidates.len() {
            self.selected_candidate_index = self.filtered_candidates.len() - 1;
        }
    }
}
