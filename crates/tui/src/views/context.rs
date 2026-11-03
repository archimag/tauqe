use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tauqe_protocol::glob::GlobPattern;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer};

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
    pub outer_docs_expanded: bool,
    pub adding_file: bool,
    pub add_access: ContextAccess,
    pub add_input: InputEditor,
    pub filtered_candidates: Vec<ContextCandidate>,
    pub total_matching: usize,
    pub selected_candidate_index: usize,
    pub confirm_clear_auto: bool,
    pub confirm_clear_user: bool,
    /// Path the developer asked to remove from the context layers and whose
    /// removal awaits the shared confirmation card. While it is set,
    /// `AppState::active_modal` reports `ActiveModal::RemoveContextFile`, so the
    /// `context/remove` request leaves the client only after the developer
    /// accepts the card and never on the bare keystroke
    /// (Conventions §4.3, §5.4).
    pub pending_remove: Option<String>,
    /// Memo of the last candidate scan.
    ///
    /// The picker derives its rows from the whole repository file list, so the
    /// answer is served from here while the query, the in-context path set, and
    /// the repository file list stay unchanged: cursor moves, focus changes, and
    /// redraws then cost nothing (#32).
    candidate_scan: CandidateScanMemo,
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
            outer_docs_expanded: true,
            adding_file: false,
            add_access: ContextAccess::ReadOnly,
            add_input: InputEditor::default(),
            filtered_candidates: Vec::new(),
            total_matching: 0,
            selected_candidate_index: 0,
            confirm_clear_auto: false,
            confirm_clear_user: false,
            pending_remove: None,
            candidate_scan: CandidateScanMemo::default(),
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
        rows.push(ContextRow::Header(ContextLayer::OuterDocs));
        if self.outer_docs_expanded {
            rows.extend(
                sorted_layer_items(items, ContextLayer::OuterDocs)
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
            ContextLayer::OuterDocs => self.outer_docs_expanded = !self.outer_docs_expanded,
            ContextLayer::Auto => self.auto_expanded = !self.auto_expanded,
        }
    }

    pub fn is_all_expanded(&self) -> bool {
        self.pinned_expanded && self.outer_docs_expanded && self.user_expanded && self.auto_expanded
    }

    pub fn expand_all(&mut self) {
        self.pinned_expanded = true;
        self.user_expanded = true;
        self.auto_expanded = true;
        self.outer_docs_expanded = true;
    }

    pub fn collapse_all(&mut self) {
        self.pinned_expanded = false;
        self.user_expanded = false;
        self.auto_expanded = false;
        self.outer_docs_expanded = false;
    }

    pub fn toggle_fold_all(&mut self) {
        if self.is_all_expanded() {
            self.collapse_all();
        } else {
            self.expand_all();
        }
    }

    /// Refreshes the picker rows for the typed query.
    ///
    /// Every call is answered from a memoized scan while the typed query, the
    /// in-context path set, and the repository file list stay unchanged, so
    /// cursor moves, focus changes, and redraws re-serve the rows the last real
    /// query edit produced instead of re-walking the whole repository file list.
    /// Only a genuine input change recomputes, exactly as `PatternMatchMemo`
    /// serves the review launch dialog (#32).
    pub fn update_filtered_candidates(&mut self, items: &[ContextItem], all_repo_files: &[String]) {
        let query = self.add_input.get_text().trim().to_string();
        let signature = candidate_scan_signature(&query, items, all_repo_files);

        if !self.candidate_scan.serves(signature) {
            self.scan_candidates(&query, items, all_repo_files);
            self.candidate_scan.record(signature);
        }

        self.settle_candidate_selection();
    }

    /// Collects the bulk-row count and the candidate rows in one traversal.
    ///
    /// Both answers the picker needs — how many files the query selects and
    /// which rows to list — come from a single pass over the repository file
    /// list; a second pass would repeat the exclusion test and the glob match of
    /// every file for the same query.
    fn scan_candidates(&mut self, query: &str, items: &[ContextItem], all_repo_files: &[String]) {
        let existing: std::collections::HashSet<&str> =
            items.iter().map(|it| it.path.as_str()).collect();

        let mut candidates = Vec::new();
        let mut total_matching = 0usize;

        if query.is_empty() {
            for file in all_repo_files {
                if existing.contains(file.as_str()) {
                    continue;
                }
                total_matching += 1;
                if candidates.len() < CANDIDATE_ROW_LIMIT {
                    candidates.push(ContextCandidate::File(file.clone()));
                }
            }
        } else {
            // The query is compiled once for the whole traversal: `GlobPattern`
            // exists for exactly this loop, and matching through it reuses the
            // precomputed pattern side instead of re-deriving it per file.
            let pattern = GlobPattern::new(query);
            let query_lower = query.to_lowercase();
            let mut matching_pattern_count = 0usize;

            for file in all_repo_files {
                if existing.contains(file.as_str()) {
                    continue;
                }
                let glob_matched = pattern.matches(file);
                if glob_matched {
                    matching_pattern_count += 1;
                }
                // A row is listed when the query selects it either as a glob or
                // as a case-insensitive substring, so the count behind the bulk
                // row and the rows themselves come from the same decision.
                if !glob_matched && !file.to_lowercase().contains(&query_lower) {
                    continue;
                }
                total_matching += 1;
                if candidates.len() < CANDIDATE_ROW_LIMIT {
                    candidates.push(ContextCandidate::File(file.clone()));
                }
            }

            let is_pattern_query = query.contains('*')
                || query.contains('?')
                || query.ends_with('/')
                || !query.contains('.');
            if matching_pattern_count > 0 && is_pattern_query {
                candidates.insert(
                    0,
                    ContextCandidate::BulkPattern {
                        pattern: query.to_string(),
                        count: matching_pattern_count,
                    },
                );
            }
        }

        self.total_matching = total_matching;
        self.filtered_candidates = candidates;
    }

    /// Applies the safe default selection over the served rows.
    ///
    /// The typed bulk row is convenient but risky: a single Enter mass-adds
    /// every match and can bloat the context without warning. The selection
    /// therefore defaults to the first concrete file so Enter is safe, while the
    /// bulk row stays reachable by navigating up.
    fn settle_candidate_selection(&mut self) {
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

/// Maximum number of concrete file rows the picker lists for one query.
///
/// The cap keeps the popup compact: a wider match is narrowed by typing one more
/// character instead of scrolling an arbitrarily long menu. The reported total
/// is never capped, so the picker still states the full size of the selection.
const CANDIDATE_ROW_LIMIT: usize = 50;

/// Memo of the last candidate scan performed by [`ContextViewState`].
///
/// Mirroring `PatternMatchMemo` in `crate::dialogs::pattern`, the picker answers
/// the same question on every typed character — which repository files the query
/// selects and which rows to list first — and the answer is fully determined by
/// the query, the in-context path set, and the repository file list. It is
/// therefore kept against a fingerprint of exactly those inputs (#32).
#[derive(Debug, Clone, Default)]
struct CandidateScanMemo {
    /// Fingerprint of the scan whose rows the picker currently holds.
    signature: Cell<Option<u64>>,
    /// Number of real scans served, so a regression test can pin that a cursor
    /// move, a focus change, or a redraw never walks the repository again.
    computations: Cell<u64>,
}

impl CandidateScanMemo {
    /// Whether the rows already on screen answer `signature`.
    fn serves(&self, signature: u64) -> bool {
        self.signature.get() == Some(signature)
    }

    /// Records that a real scan answered `signature`.
    fn record(&self, signature: u64) {
        self.signature.set(Some(signature));
        let computations = self.computations.get();
        self.computations.set(computations.saturating_add(1));
    }

    /// Number of real scans served since the picker was built.
    ///
    /// Only the regression test reads this, exactly like `PatternMatchMemo` in
    /// `crate::dialogs::pattern`, so the allowance keeps the zero-warning gate
    /// green instead of widening the module surface for a counter.
    #[allow(dead_code)]
    fn computations(&self) -> u64 {
        self.computations.get()
    }
}

/// Fingerprint of the inputs behind a memoized candidate scan.
///
/// Mirroring `patterns_signature` in `crate::dialogs::pattern`, the typed query,
/// the context paths, and the repository file list are folded into one hash. The
/// repository list is folded by content rather than by its address: the daemon
/// replaces the list wholesale on a refresh, and re-listing a repository of the
/// same size routinely hands the new vector the block the previous one released,
/// so an address check would serve a stale candidate list after a rename. Folding
/// the paths stays far cheaper than the glob traversal the fingerprint guards.
fn candidate_scan_signature(query: &str, items: &[ContextItem], all_repo_files: &[String]) -> u64 {
    let mut hasher = DefaultHasher::new();
    query.hash(&mut hasher);
    items.len().hash(&mut hasher);
    for item in items {
        item.path.hash(&mut hasher);
    }
    all_repo_files.len().hash(&mut hasher);
    for file in all_repo_files {
        file.hash(&mut hasher);
    }
    hasher.finish()
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
            item("docs.md", ContextLayer::OuterDocs),
            item("auto.rs", ContextLayer::Auto),
        ];

        let rows = view.compute_rows(&items);
        assert_eq!(
            rendered_paths(&rows),
            vec!["pinned.rs", "docs.md", "user.rs", "auto.rs"]
        );

        view.collapse_all();
        let folded = view.compute_rows(&items);
        assert!(rendered_paths(&folded).is_empty());
    }

    #[test]
    fn test_the_outer_docs_section_is_folded_independently() {
        let mut view = ContextViewState::default();
        let items = vec![
            item("pinned.rs", ContextLayer::Pinned),
            item("docs.md", ContextLayer::OuterDocs),
            item("user.rs", ContextLayer::User),
        ];

        assert!(view.is_all_expanded());
        view.toggle_section(ContextLayer::OuterDocs);
        assert!(!view.outer_docs_expanded);
        assert_eq!(
            rendered_paths(&view.compute_rows(&items)),
            vec!["pinned.rs", "user.rs"]
        );

        view.toggle_section(ContextLayer::OuterDocs);
        assert_eq!(
            rendered_paths(&view.compute_rows(&items)),
            vec!["pinned.rs", "docs.md", "user.rs"]
        );
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
    /// The picker scan runs once per (query, context, repository) state: cursor
    /// moves, focus changes, and redraws re-serve the stored rows, and only a
    /// real input change recomputes (#32).
    #[test]
    fn update_filtered_candidates_is_memoized_on_the_scan_signature() {
        let mut view = ContextViewState::default();
        let repo = vec!["src/lib.rs".to_string(), "README.md".to_string()];
        view.add_input.insert_str("*.rs");

        view.update_filtered_candidates(&[], &repo);
        assert_eq!(view.candidate_scan.computations(), 1);
        assert_eq!(view.total_matching, 1);
        assert_eq!(view.selected_candidate_index, 1);

        // Moving up to the bulk row and redrawing never re-walks the repository:
        // the stored rows still answer the unchanged query, and the safe default
        // selection is re-applied exactly as a fresh scan would apply it.
        view.selected_candidate_index = 0;
        view.update_filtered_candidates(&[], &repo);
        assert_eq!(view.candidate_scan.computations(), 1);
        assert_eq!(view.selected_candidate_index, 1);

        // A real query edit recomputes.
        view.add_input.clear();
        view.add_input.insert_str("*.md");
        view.update_filtered_candidates(&[], &repo);
        assert_eq!(view.candidate_scan.computations(), 2);
        assert_eq!(view.total_matching, 1);

        // A context change recomputes: the added path leaves the candidate set,
        // so the same query now selects nothing at all.
        view.add_input.clear();
        view.add_input.insert_str("*.rs");
        let items = vec![item("src/lib.rs", ContextLayer::User)];
        view.update_filtered_candidates(&items, &repo);
        assert_eq!(view.candidate_scan.computations(), 3);
        assert_eq!(view.total_matching, 0);
        assert!(view.filtered_candidates.is_empty());
        assert_eq!(view.selected_candidate_index, 0);

        // A refreshed repository file list recomputes even when the replaced one
        // had the same length, which a pointer-identity check would miss.
        let refreshed = vec!["src/main.rs".to_string(), "README.md".to_string()];
        view.update_filtered_candidates(&items, &refreshed);
        assert_eq!(view.candidate_scan.computations(), 4);
        assert_eq!(view.total_matching, 1);
        assert_eq!(
            view.filtered_candidates.last(),
            Some(&ContextCandidate::File("src/main.rs".to_string()))
        );
    }
}
