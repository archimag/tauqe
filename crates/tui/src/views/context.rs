use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use tauqe_protocol::glob::GlobPattern;
use tauqe_protocol::{ContextAccess, ContextItem, ContextLayer, OuterDocsCacheEntry};

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

/// Browsing state of the Context view.
///
/// The view owns the section folds, the cursor, and the inline path input. The
/// confirmations it raises — the two layer clears and the single-file removal —
/// are modal surfaces that outrank the view while they are open, so they live in
/// [`crate::app::ModalState`] instead (Conventions §5.4, §7).
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
    /// in-context path set, and the repository file list revision stay unchanged,
    /// so cursor moves, focus changes, and redraws re-serve the rows the last real
    /// query edit produced instead of re-walking the whole repository file list.
    /// The revision is the authoritative repository signal: the list itself is
    /// replaced only through `AppState::set_repo_files`, which advances it in the
    /// same step, so the memo never hashes a path list that can hold a whole
    /// monorepo. Only a genuine input change recomputes, exactly as
    /// `PatternMatchMemo` serves the review launch dialog (#32).
    pub fn update_filtered_candidates(
        &mut self,
        items: &[ContextItem],
        all_repo_files: &[String],
        repo_revision: u64,
    ) {
        let query = self.add_input.get_text().trim().to_string();
        let signature = candidate_scan_signature(&query, items, repo_revision);

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
/// One row of the external documentation cache tree.
///
/// A row addresses a document by its logical cache path — the path the context
/// layer and the prompt already carry — so folding, clicking, and removing a
/// node never expose the disk prefix the snapshots are stored under
/// (c-outer-docs-logical-paths, c-outer-docs-cache-tui-browser).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTreeRow {
    /// Nesting depth; the renderer derives the indent from it.
    pub depth: usize,
    /// Path segment the row displays.
    pub label: String,
    /// Logical cache path the row addresses.
    pub path: String,
    /// Whether the row groups documents or names one document.
    pub kind: CacheTreeKind,
    /// Documents the row covers, counted recursively for a directory.
    pub files: usize,
    /// Bytes the row covers, summed for a directory.
    pub size_bytes: u64,
    /// Tokens the row covers, summed for a directory.
    pub estimated_tokens: u64,
    /// Whether a directory is folded; always `false` for a document.
    pub collapsed: bool,
}

/// Kind of one external documentation cache tree row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTreeKind {
    /// A directory grouping the documents below one logical prefix.
    Directory,
    /// One cached document.
    Document,
}

/// Nested inventory of one logical cache directory.
#[derive(Debug, Default)]
struct CacheDir {
    dirs: std::collections::BTreeMap<String, CacheDir>,
    files: std::collections::BTreeMap<String, (u64, u64)>,
}

impl CacheDir {
    /// Inserts one document by the segments of its logical path.
    fn insert(&mut self, segments: &[&str], size_bytes: u64, estimated_tokens: u64) {
        let Some((leaf, parents)) = segments.split_last() else {
            return;
        };
        let mut current = self;
        for segment in parents {
            current = current.dirs.entry((*segment).to_string()).or_default();
        }
        current
            .files
            .insert((*leaf).to_string(), (size_bytes, estimated_tokens));
    }

    /// Documents, bytes, and tokens the directory covers, recursively.
    fn totals(&self) -> (usize, u64, u64) {
        let mut files = self.files.len();
        let mut size_bytes: u64 = self.files.values().map(|(size, _)| *size).sum();
        let mut estimated_tokens: u64 = self.files.values().map(|(_, tokens)| *tokens).sum();
        for child in self.dirs.values() {
            let (child_files, child_size, child_tokens) = child.totals();
            files += child_files;
            size_bytes += child_size;
            estimated_tokens += child_tokens;
        }
        (files, size_bytes, estimated_tokens)
    }
}

/// Joins one path segment onto the logical prefix of its parent.
fn join_cache_path(prefix: &str, label: &str) -> String {
    if prefix.is_empty() {
        label.to_string()
    } else {
        format!("{prefix}/{label}")
    }
}

/// Flattens one directory into rows, respecting the folded prefixes.
fn flatten_cache_dir(
    dir: &CacheDir,
    depth: usize,
    prefix: &str,
    collapsed: &std::collections::HashSet<String>,
    rows: &mut Vec<CacheTreeRow>,
) {
    for (label, child) in &dir.dirs {
        let path = join_cache_path(prefix, label);
        let (files, size_bytes, estimated_tokens) = child.totals();
        let folded = collapsed.contains(&path);
        rows.push(CacheTreeRow {
            depth,
            label: label.clone(),
            path: path.clone(),
            kind: CacheTreeKind::Directory,
            files,
            size_bytes,
            estimated_tokens,
            collapsed: folded,
        });
        if !folded {
            flatten_cache_dir(child, depth.saturating_add(1), &path, collapsed, rows);
        }
    }
    for (label, (size_bytes, estimated_tokens)) in &dir.files {
        rows.push(CacheTreeRow {
            depth,
            label: label.clone(),
            path: join_cache_path(prefix, label),
            kind: CacheTreeKind::Document,
            files: 1,
            size_bytes: *size_bytes,
            estimated_tokens: *estimated_tokens,
            collapsed: false,
        });
    }
}

/// Projects the flat cache inventory into the foldable rows of the browser.
///
/// Directories come before the documents they hold and every level is ordered
/// alphabetically, so identical inventories always project into identical rows
/// (c-outer-docs-cache-tui-browser).
pub fn cache_tree_rows(
    entries: &[OuterDocsCacheEntry],
    collapsed: &std::collections::HashSet<String>,
) -> Vec<CacheTreeRow> {
    let mut root = CacheDir::default();
    for entry in entries {
        let segments: Vec<&str> = entry
            .path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        root.insert(&segments, entry.size_bytes, entry.estimated_tokens);
    }
    let mut rows = Vec::new();
    flatten_cache_dir(&root, 0, "", collapsed, &mut rows);
    rows
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
/// the query, the in-context path set, and the revision of the repository file
/// list. It is therefore kept against a fingerprint of exactly those inputs
/// (#32).
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
/// the context paths, and the repository file list revision are folded into one
/// hash. The path list itself is never folded: it can hold a whole monorepo, and
/// hashing it on every keystroke and every context event costs exactly the pass
/// the memo exists to avoid. The revision is authoritative because the list has
/// a single writer — `AppState::set_repo_files` replaces the list and advances
/// the counter in the same step — so a refreshed list is always a distinct
/// fingerprint and a re-served answer always describes the list in hand (#32).
fn candidate_scan_signature(query: &str, items: &[ContextItem], repo_revision: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    query.hash(&mut hasher);
    items.len().hash(&mut hasher);
    for item in items {
        item.path.hash(&mut hasher);
    }
    repo_revision.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

        view.update_filtered_candidates(&[], &all_repo_files, 1);

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

        view.update_filtered_candidates(&[], &all_repo_files, 1);

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

        view.update_filtered_candidates(&[], &all_repo_files, 1);

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

        view.update_filtered_candidates(&[], &all_repo_files, 1);

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
    /// The picker scan runs once per (query, context, repository revision)
    /// state: cursor moves, focus changes, and redraws re-serve the stored rows,
    /// and only a real input change or a replaced repository list recomputes
    /// (#32).
    #[test]
    fn update_filtered_candidates_is_memoized_on_the_scan_signature() {
        let mut view = ContextViewState::default();
        let repo = vec!["src/lib.rs".to_string(), "README.md".to_string()];
        view.add_input.insert_str("*.rs");

        view.update_filtered_candidates(&[], &repo, 1);
        assert_eq!(view.candidate_scan.computations(), 1);
        assert_eq!(view.total_matching, 1);
        assert_eq!(view.selected_candidate_index, 1);

        // Moving up to the bulk row and redrawing never re-walks the repository:
        // the stored rows still answer the unchanged query, and the safe default
        // selection is re-applied exactly as a fresh scan would apply it.
        view.selected_candidate_index = 0;
        view.update_filtered_candidates(&[], &repo, 1);
        assert_eq!(view.candidate_scan.computations(), 1);
        assert_eq!(view.selected_candidate_index, 1);

        // A real query edit recomputes.
        view.add_input.clear();
        view.add_input.insert_str("*.md");
        view.update_filtered_candidates(&[], &repo, 1);
        assert_eq!(view.candidate_scan.computations(), 2);
        assert_eq!(view.total_matching, 1);

        // A context change recomputes: the added path leaves the candidate set,
        // so the same query now selects nothing at all.
        view.add_input.clear();
        view.add_input.insert_str("*.rs");
        let items = vec![item("src/lib.rs", ContextLayer::User)];
        view.update_filtered_candidates(&items, &repo, 1);
        assert_eq!(view.candidate_scan.computations(), 3);
        assert_eq!(view.total_matching, 0);
        assert!(view.filtered_candidates.is_empty());
        assert_eq!(view.selected_candidate_index, 0);

        // A refreshed repository file list advances the revision that keys the
        // memo, so the rows are rebuilt even when the replaced list kept its
        // length and every path of it.
        let refreshed = vec!["src/main.rs".to_string(), "README.md".to_string()];
        view.update_filtered_candidates(&items, &refreshed, 2);
        assert_eq!(view.candidate_scan.computations(), 4);
        assert_eq!(view.total_matching, 1);
        assert_eq!(
            view.filtered_candidates.last(),
            Some(&ContextCandidate::File("src/main.rs".to_string()))
        );
    }
    fn cache_entry(path: &str, size_bytes: u64, estimated_tokens: u64) -> OuterDocsCacheEntry {
        OuterDocsCacheEntry {
            path: path.to_string(),
            size_bytes,
            estimated_tokens,
        }
    }

    /// One directory row per identifier prefix, one row per document, with
    /// directories first and every level alphabetical whatever order the
    /// inventory arrives in (c-outer-docs-cache-tui-browser).
    #[test]
    fn cache_tree_projects_directories_before_their_documents() {
        let entries = vec![
            cache_entry("openrouter/tts", 2048, 512),
            cache_entry("openrouter/chat-completion", 4096, 1024),
            cache_entry("rust-book/ch04", 1024, 128),
        ];

        let rows = cache_tree_rows(&entries, &HashSet::new());

        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["openrouter", "chat-completion", "tts", "rust-book", "ch04"]
        );
        assert_eq!(rows[0].kind, CacheTreeKind::Directory);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].files, 2);
        assert_eq!(rows[0].estimated_tokens, 1536);
        assert_eq!(rows[1].kind, CacheTreeKind::Document);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[1].path, "openrouter/chat-completion");
    }

    /// A folded directory hides its subtree while keeping its own aggregate
    /// row, so a collapsed identifier still states what it covers.
    #[test]
    fn cache_tree_folds_the_prefix_it_was_given() {
        let entries = vec![
            cache_entry("openrouter/tts", 2048, 512),
            cache_entry("openrouter/chat-completion", 4096, 1024),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert("openrouter".to_string());

        let rows = cache_tree_rows(&entries, &collapsed);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, CacheTreeKind::Directory);
        assert!(rows[0].collapsed);
        assert_eq!(rows[0].files, 2);
        assert_eq!(rows[0].size_bytes, 6144);
    }

    /// A document directly under the cache root stays a document row instead of
    /// being folded into a phantom directory.
    #[test]
    fn cache_tree_keeps_a_root_document_a_document() {
        let rows = cache_tree_rows(&[cache_entry("index", 16, 4)], &HashSet::new());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, CacheTreeKind::Document);
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[0].path, "index");
    }
}
