use tauqe_protocol::{matches_glob_pattern, ContextAccess, ContextItem, ContextLayer};

use crate::editor::InputEditor;

pub mod input;
pub mod render;

pub use self::input::{handle_context_key, CONTEXT_COMMAND_MAP};
pub use self::render::render_context_view;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextRow {
    Header(ContextLayer),
    Item(ContextItem),
}

/// One row of the Context file picker.
///
/// The bulk mass-add action and concrete repository paths are two different
/// kinds of rows. The enum makes that distinction a typed fact instead of a
/// string-prefix probe (Conventions 1.5): the Enter/Tab dispatch and the row
/// renderer match variants and never inspect row text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCandidate {
    /// Synthetic bulk row: mass-add every repository file matching `pattern`.
    BulkPattern { pattern: String, count: usize },
    /// A concrete repository file path.
    File(String),
}

impl ContextCandidate {
    /// Display text of the picker row.
    ///
    /// The `[+] Add all matching` wording is derived here and nowhere else,
    /// so the rendered label can never diverge from the typed dispatch
    /// decision that consumes the same variant.
    pub fn display_label(&self) -> String {
        match self {
            Self::BulkPattern { pattern, count } => {
                format!("[+] Add all matching '{}' ({} files)", pattern, count)
            }
            Self::File(path) => path.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextViewState {
    pub cursor_index: usize,
    pub scroll: u16,
    pub view_height: usize,
    pub pinned_expanded: bool,
    pub user_expanded: bool,
    pub auto_expanded: bool,
    pub adding_file: bool,
    pub add_access: ContextAccess,
    pub add_input: InputEditor,
    pub filtered_candidates: Vec<ContextCandidate>,
    pub total_matching: usize,
    pub selected_candidate_index: usize,
    pub confirm_clear_auto: bool,
    pub confirm_clear_user: bool,
}

impl Default for ContextViewState {
    fn default() -> Self {
        Self {
            cursor_index: 0,
            scroll: 0,
            view_height: 10,
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
            confirm_clear_user: false,
        }
    }
}

/// Projects one context layer into rows ordered by path.
///
/// The context manager keeps files in insertion order so the prompt prefix
/// stays stable between rounds of a turn. The interface still has to be
/// predictable to browse, so alphabetical ordering is applied here only, at
/// the projection step, and never in the domain layer.
fn sorted_layer_items(items: &[ContextItem], layer: ContextLayer) -> Vec<&ContextItem> {
    let mut layer_items: Vec<&ContextItem> = items.iter().filter(|i| i.layer == layer).collect();
    layer_items.sort_by(|a, b| a.path.cmp(&b.path));
    layer_items
}

impl ContextViewState {
    pub fn compute_rows(&self, items: &[ContextItem]) -> Vec<ContextRow> {
        let mut rows = Vec::new();

        rows.push(ContextRow::Header(ContextLayer::Pinned));
        if self.pinned_expanded {
            rows.extend(
                sorted_layer_items(items, ContextLayer::Pinned)
                    .into_iter()
                    .map(|it| ContextRow::Item(it.clone())),
            );
        }

        rows.push(ContextRow::Header(ContextLayer::User));
        if self.user_expanded {
            rows.extend(
                sorted_layer_items(items, ContextLayer::User)
                    .into_iter()
                    .map(|it| ContextRow::Item(it.clone())),
            );
        }

        rows.push(ContextRow::Header(ContextLayer::Auto));
        if self.auto_expanded {
            rows.extend(
                sorted_layer_items(items, ContextLayer::Auto)
                    .into_iter()
                    .map(|it| ContextRow::Item(it.clone())),
            );
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

    pub fn is_all_expanded(&self) -> bool {
        self.pinned_expanded && self.user_expanded && self.auto_expanded
    }

    pub fn expand_all(&mut self) {
        self.pinned_expanded = true;
        self.user_expanded = true;
        self.auto_expanded = true;
    }

    pub fn collapse_all(&mut self) {
        self.pinned_expanded = false;
        self.user_expanded = false;
        self.auto_expanded = false;
    }

    pub fn toggle_fold_all(&mut self) {
        if self.is_all_expanded() {
            self.collapse_all();
        } else {
            self.expand_all();
        }
    }

    pub fn update_filtered_candidates(&mut self, items: &[ContextItem], all_repo_files: &[String]) {
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
                candidates.push(ContextCandidate::BulkPattern {
                    pattern: query.to_string(),
                    count: matching_pattern_count,
                });
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
            candidates.extend(all_matches.into_iter().take(50).map(ContextCandidate::File));
        } else {
            let available: Vec<String> = all_repo_files
                .iter()
                .filter(|f| !existing.contains(f.as_str()))
                .cloned()
                .collect();
            self.total_matching = available.len();
            candidates = available
                .into_iter()
                .take(50)
                .map(ContextCandidate::File)
                .collect();
        }

        self.filtered_candidates = candidates;

        // The typed bulk row is convenient but risky: a single Enter mass-adds
        // every match and can bloat the context without warning. Default the
        // selection to the first concrete file so Enter is safe, while keeping
        // the bulk row reachable by navigating up.
        let bulk_row_first = matches!(
            self.filtered_candidates.first(),
            Some(ContextCandidate::BulkPattern { .. })
        );

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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, layer: ContextLayer) -> ContextItem {
        ContextItem {
            path: path.to_string(),
            access: ContextAccess::ReadOnly,
            size_bytes: 16,
            estimated_tokens: 4,
            layer,
            insertion_seq: 0,
        }
    }

    fn rendered_paths(rows: &[ContextRow]) -> Vec<String> {
        rows.iter()
            .filter_map(|row| match row {
                ContextRow::Item(it) => Some(it.path.clone()),
                ContextRow::Header(_) => None,
            })
            .collect()
    }

    #[test]
    fn test_compute_rows_sorts_files_alphabetically_within_layer() {
        let view = ContextViewState::default();
        // Insertion order contradicts alphabetical order.
        let items = vec![
            item("zeta.rs", ContextLayer::User),
            item("alpha.rs", ContextLayer::User),
            item("mid.rs", ContextLayer::User),
        ];

        let rows = view.compute_rows(&items);
        assert_eq!(rendered_paths(&rows), vec!["alpha.rs", "mid.rs", "zeta.rs"]);
    }

    #[test]
    fn test_sentinel_prefixed_path_stays_a_file_candidate() {
        let mut view = ContextViewState::default();
        // A real repository path may literally start with the old sentinel
        // prefix; the typed model keeps it a concrete file row.
        let suspicious = "[+] Add all matching '*.rs' (3 files)";
        let all_repo_files = vec![suspicious.to_string(), "src/lib.rs".to_string()];

        view.update_filtered_candidates(&[], &all_repo_files);

        assert_eq!(
            view.filtered_candidates.first(),
            Some(&ContextCandidate::File(suspicious.to_string()))
        );
    }

    #[test]
    fn test_pattern_query_produces_typed_bulk_row_and_skips_it() {
        let mut view = ContextViewState::default();
        let all_repo_files = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        view.add_input.insert_str("*.rs");

        view.update_filtered_candidates(&[], &all_repo_files);

        assert_eq!(
            view.filtered_candidates.first(),
            Some(&ContextCandidate::BulkPattern {
                pattern: "*.rs".to_string(),
                count: 2,
            })
        );
        // Enter stays safe: the default selection is the first concrete file.
        assert_eq!(
            view.filtered_candidates.get(view.selected_candidate_index),
            Some(&ContextCandidate::File("src/lib.rs".to_string()))
        );
    }

    #[test]
    fn test_compute_rows_keeps_layer_sections_and_respects_folding() {
        let mut view = ContextViewState::default();
        let items = vec![
            item("pinned.rs", ContextLayer::Pinned),
            item("user.rs", ContextLayer::User),
            item("auto.rs", ContextLayer::Auto),
        ];

        let rows = view.compute_rows(&items);
        assert_eq!(
            rendered_paths(&rows),
            vec!["pinned.rs", "user.rs", "auto.rs"]
        );

        view.collapse_all();
        let folded = view.compute_rows(&items);
        assert!(rendered_paths(&folded).is_empty());
    }

    #[test]
    fn update_filtered_candidates_produces_a_typed_bulk_row_for_a_pattern_query() {
        let mut view = ContextViewState::default();
        view.add_input.insert_str("*.rs");
        let all_repo_files = vec!["a.rs".to_string(), "b.rs".to_string()];

        view.update_filtered_candidates(&[], &all_repo_files);

        assert_eq!(
            view.filtered_candidates,
            vec![
                ContextCandidate::BulkPattern {
                    pattern: "*.rs".to_string(),
                    count: 2,
                },
                ContextCandidate::File("a.rs".to_string()),
                ContextCandidate::File("b.rs".to_string()),
            ]
        );
        assert_eq!(view.total_matching, 2);
        // Enter stays safe: the default selection is the first concrete file.
        assert_eq!(view.selected_candidate_index, 1);
    }

    /// A repository path may literally begin with the historical sentinel
    /// prefix. Row kind is carried by the variant, never recovered from the
    /// text, so such a path stays a plain file row.
    #[test]
    fn update_filtered_candidates_keeps_a_sentinel_prefixed_path_a_plain_file_row() {
        let sentinel_path = "[+] Add all matching 'demo.rs' (2 files)".to_string();
        let mut view = ContextViewState::default();
        view.add_input.insert_str("demo.rs");
        let all_repo_files = vec![sentinel_path.clone()];

        view.update_filtered_candidates(&[], &all_repo_files);

        assert_eq!(
            view.filtered_candidates,
            vec![ContextCandidate::File(sentinel_path)]
        );
        let has_bulk_row = view
            .filtered_candidates
            .iter()
            .any(|candidate| matches!(candidate, ContextCandidate::BulkPattern { .. }));
        assert!(!has_bulk_row);
    }

    #[test]
    fn bulk_row_display_label_is_derived_from_the_variant() {
        let bulk = ContextCandidate::BulkPattern {
            pattern: "*.rs".to_string(),
            count: 3,
        };
        assert_eq!(
            bulk.display_label(),
            "[+] Add all matching '*.rs' (3 files)"
        );
        assert_eq!(
            ContextCandidate::File("x.rs".to_string()).display_label(),
            "x.rs"
        );
    }
}
