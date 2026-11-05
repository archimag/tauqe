use std::collections::HashMap;

use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::theme::engine::{current_engine, ThemeEngine};

use super::state::{EditOp, StreamingFileEdit, StreamingHunk};

/// Cache of rendered diff bodies for the staged file edits.
///
/// Computing a `similar::TextDiff` is comparatively expensive and would
/// otherwise be repeated for the audited file on every frame and navigation
/// key. Entries are keyed by file path and invalidated by a cheap fingerprint
/// of the file's hunks, so a changed hunk always re-renders.
/// The cache travels with the modal subsystem: the overlay state owns an
/// imported commit audit together with the rendered bodies of its files, and
/// that state derives `Debug` and `Clone` like every other surface it stores.
#[derive(Debug, Default, Clone)]
pub struct DiffRenderCache {
    entries: HashMap<String, DiffCacheEntry>,
}

#[derive(Debug, Clone)]
struct DiffCacheEntry {
    fingerprint: u64,
    rendered: RenderedDiff,
}

/// A rendered diff body together with the row offsets of its hunk headers and
/// the line coordinates of every row.
///
/// The offsets are relative to `lines`, so the Viewport projects them through
/// its own visual coordinates and names the hunk under the top edge without
/// re-deriving the patch layout (c-1). `row_numbers` runs parallel to `lines`
/// and names the lines every row occupies on both sides of the patch, so a
/// two-column gutter reads the coordinates of the patch instead of counting the
/// rows of the patch itself (c-two-column-gutter).
#[derive(Debug, Default, Clone)]
pub(crate) struct RenderedDiff {
    pub lines: Vec<Line<'static>>,
    pub hunk_rows: Vec<usize>,
    pub row_numbers: Vec<RowNumbers>,
}

/// Source and result line number of one rendered diff row.
///
/// A context row carries both sides, a removed row only the line it occupied in
/// the file before the patch, and an added row only the line it occupies in the
/// file after it. A hunk header carries neither. `None` therefore means the row
/// has no line on that side, or the hunk did not state its coordinates, and the
/// gutter leaves that column blank instead of inventing a number
/// (c-two-column-gutter).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RowNumbers {
    pub old: Option<usize>,
    pub new: Option<usize>,
}

impl DiffRenderCache {
    /// Renders the diff body together with the row offsets of its hunk headers.
    ///
    /// The Viewport needs those offsets to name the hunk the top edge sits
    /// inside, so the overlay reuses the very rows that were drawn instead of
    /// re-deriving them from the patch text (c-1).
    ///
    /// The body is served by reference: a cache hit costs one fingerprint pass
    /// and a borrow, and a freshly rendered body is served straight from the
    /// entry it was inserted into, so neither path copies the rendered patch
    /// (c-1).
    pub(crate) fn render(&mut self, file: &StreamingFileEdit) -> &RenderedDiff {
        let fingerprint = diff_fingerprint(file);
        let entry = self
            .entries
            .entry(file.path.clone())
            .and_modify(|entry| {
                if entry.fingerprint != fingerprint {
                    entry.fingerprint = fingerprint;
                    entry.rendered = render_expanded_diff(file);
                }
            })
            .or_insert_with(|| DiffCacheEntry {
                fingerprint,
                rendered: render_expanded_diff(file),
            });
        &entry.rendered
    }

    /// Discards the cached diff body of a single path.
    ///
    /// A repeated edit of the same file restarts its hunk stream from scratch:
    /// the diff rendered for the previous attempt must never be reused, or the
    /// viewport would keep showing fragments of a patch that was rejected and
    /// retried (c-stream-file-reset).
    pub fn invalidate(&mut self, path: &str) {
        self.entries.remove(path);
    }
}

/// Cheap content fingerprint of a file's hunks, used to invalidate the diff
/// render cache without recomputing the diff itself.
///
/// The coordinates and the context hint belong to the fingerprint because a
/// header and a gutter are rendered from them: a hunk that keeps its texts but
/// lands on another line must re-render (c-diff-cache-git-headers).
fn diff_fingerprint(file: &StreamingFileEdit) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    file.path.hash(&mut hasher);
    (file.op_type as u8).hash(&mut hasher);
    for hunk in &file.hunks {
        hunk.hunk_index.hash(&mut hasher);
        hunk.old_start.hash(&mut hasher);
        hunk.old_lines.hash(&mut hasher);
        hunk.new_start.hash(&mut hasher);
        hunk.new_lines.hash(&mut hasher);
        hunk.context_hint.hash(&mut hasher);
        hunk.old_text.hash(&mut hasher);
        hunk.new_text.hash(&mut hasher);
    }
    hasher.finish()
}

/// Renders the expanded diff body for a single file edit.
///
/// Extracted from the model-line builder so the result can be cached and its
/// length reused for scroll accounting without recomputing the diff.
fn render_expanded_diff(file: &StreamingFileEdit) -> RenderedDiff {
    let engine = current_engine();
    let mut rendered = RenderedDiff::default();
    for hunk in &file.hunks {
        rendered.hunk_rows.push(rendered.lines.len());
        rendered
            .lines
            .push(hunk_header(file.op_type, hunk, &engine));
        rendered.row_numbers.push(RowNumbers::default());
        match file.op_type {
            EditOp::Create => push_created_rows(&mut rendered, hunk, &engine),
            EditOp::Delete => push_deleted_rows(&mut rendered, hunk, &engine),
            _ => push_replaced_rows(&mut rendered, hunk, &engine),
        }
    }
    rendered
}

/// Renders the canonical unified diff header of one hunk.
///
/// Every hunk states the exact lines it spans on both sides in the standard
/// `@@ -old_start,old_lines +new_start,new_lines @@` form, so a header names its
/// coordinates instead of its position inside the patch. The section hint the
/// patch carries follows the ranges. A whole-file creation or deletion keeps its
/// operation marker in that hint slot, because `new file` and `deleted file`
/// say more about the row than a context line would
/// (c-diff-cache-git-headers).
fn hunk_header(op: EditOp, hunk: &StreamingHunk, engine: &ThemeEngine) -> Line<'static> {
    let palette = engine.palette();
    let ranges = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
    );
    let hint = match op {
        EditOp::Create => Some("new file"),
        EditOp::Delete => Some("deleted file"),
        _ => hunk.context_hint.as_deref(),
    };
    let mut spans = vec![Span::styled(
        format!("  {ranges}"),
        engine.fg(palette.status_info).bold(),
    )];
    if let Some(hint) = hint {
        spans.push(Span::styled(
            format!(" {hint}"),
            engine.fg(palette.text_secondary),
        ));
    }
    Line::from(spans)
}

/// Appends the rows of a whole-file creation: every line is an addition, so the
/// source column stays empty and the result column names the new lines
/// (c-two-column-gutter).
fn push_created_rows(rendered: &mut RenderedDiff, hunk: &StreamingHunk, engine: &ThemeEngine) {
    let color = engine.fg(engine.palette().diff_added);
    let mut new_number = line_number(hunk.new_start);
    for text in hunk.new_text.lines() {
        rendered.lines.push(Line::from(vec![
            Span::styled("  + ", color.bold()),
            Span::styled(text.to_string(), color),
        ]));
        rendered.row_numbers.push(RowNumbers {
            old: None,
            new: new_number,
        });
        new_number = new_number.map(|number| number + 1);
    }
}

/// Appends the rows of a whole-file deletion: every line is a removal, so the
/// result column stays empty and the source column names the removed lines
/// (c-two-column-gutter).
fn push_deleted_rows(rendered: &mut RenderedDiff, hunk: &StreamingHunk, engine: &ThemeEngine) {
    let color = engine.fg(engine.palette().diff_removed);
    let mut old_number = line_number(hunk.old_start);
    for text in hunk.old_text.lines() {
        rendered.lines.push(Line::from(vec![
            Span::styled("  - ", color.bold()),
            Span::styled(text.to_string(), color),
        ]));
        rendered.row_numbers.push(RowNumbers {
            old: old_number,
            new: None,
        });
        old_number = old_number.map(|number| number + 1);
    }
}

/// Appends the rows of a hunk that rewrites the file in place.
///
/// The rows follow the operation tags of the diff: a context row keeps both
/// counters, a removal advances the source counter, and an addition advances the
/// result counter, so every row is numbered exactly as the patch positions it
/// (c-two-column-gutter).
fn push_replaced_rows(rendered: &mut RenderedDiff, hunk: &StreamingHunk, engine: &ThemeEngine) {
    let palette = engine.palette();
    let diff = similar::TextDiff::from_lines(&hunk.old_text, &hunk.new_text);
    let mut old_number = line_number(hunk.old_start);
    let mut new_number = line_number(hunk.new_start);
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let text = change.value().trim_end_matches(['\r', '\n']);
            let (prefix, color, numbers) = match change.tag() {
                similar::ChangeTag::Delete => {
                    let numbers = RowNumbers {
                        old: old_number,
                        new: None,
                    };
                    old_number = old_number.map(|number| number + 1);
                    ("  - ", engine.fg(palette.diff_removed), numbers)
                }
                similar::ChangeTag::Insert => {
                    let numbers = RowNumbers {
                        old: None,
                        new: new_number,
                    };
                    new_number = new_number.map(|number| number + 1);
                    ("  + ", engine.fg(palette.diff_added), numbers)
                }
                similar::ChangeTag::Equal => {
                    let numbers = RowNumbers {
                        old: old_number,
                        new: new_number,
                    };
                    old_number = old_number.map(|number| number + 1);
                    new_number = new_number.map(|number| number + 1);
                    ("    ", engine.fg(palette.diff_context), numbers)
                }
            };
            rendered.lines.push(Line::from(vec![
                Span::styled(prefix, color.bold()),
                Span::styled(text.to_string(), color),
            ]));
            rendered.row_numbers.push(numbers);
        }
    }
}

/// First line number of one side of a hunk.
///
/// A hunk whose coordinates are unknown keeps zeros, so `0` means "unknown"
/// rather than a real line and that side is never numbered from zero
/// (c-two-column-gutter).
fn line_number(start: usize) -> Option<usize> {
    (start > 0).then_some(start)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    use crate::views::repl::state::{FileEditStatus, StreamingHunk};

    fn file_with_hunk(path: &str) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: EditOp::Replace,
            status: FileEditStatus::Ok,
            error: None,
            hunks: vec![StreamingHunk {
                hunk_index: 0,
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                context_hint: None,
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            }],
            retry_info: None,
        }
    }

    #[test]
    fn invalidate_drops_only_the_restarted_path() {
        let mut cache = DiffRenderCache::default();
        let kept = file_with_hunk("crates/tui/src/main.rs");
        let retried = file_with_hunk("crates/tui/src/app.rs");

        assert!(!cache.render(&kept).lines.is_empty());
        assert!(!cache.render(&retried).lines.is_empty());
        assert_eq!(cache.entries.len(), 2);

        cache.invalidate("crates/tui/src/app.rs");

        assert!(cache.entries.contains_key("crates/tui/src/main.rs"));
        assert!(
            !cache.entries.contains_key("crates/tui/src/app.rs"),
            "the stale diff of the restarted file must be dropped"
        );
    }

    #[test]
    fn invalidate_ignores_an_unknown_path() {
        let mut cache = DiffRenderCache::default();
        cache.invalidate("crates/tui/src/never_rendered.rs");
        assert!(cache.entries.is_empty());
    }

    /// The point of the borrowed return: an unchanged file is served in place,
    /// so a frame that re-reads the same patch pays one fingerprint pass and a
    /// borrow instead of a copy of the rendered body (c-1).
    #[test]
    fn an_unchanged_file_is_served_by_reference() {
        let mut cache = DiffRenderCache::default();
        let file = file_with_hunk("crates/tui/src/main.rs");

        let first = cache.render(&file) as *const RenderedDiff;
        let second = cache.render(&file) as *const RenderedDiff;

        assert_eq!(
            first, second,
            "an unchanged file must be served straight out of its cache entry"
        );
        assert_eq!(cache.entries.len(), 1);
    }

    /// The complementary case guards the refresh path: a changed hunk must be
    /// re-rendered instead of serving the stale body, and the entry must record
    /// the fingerprint of the body it now serves (c-1).
    #[test]
    fn a_changed_hunk_refreshes_the_stored_diff() {
        let mut cache = DiffRenderCache::default();
        let mut file = file_with_hunk("crates/tui/src/app.rs");

        assert!(rendered_text(cache.render(&file)).contains("let a = 2;"));
        assert_eq!(
            cache
                .entries
                .get("crates/tui/src/app.rs")
                .map(|entry| entry.fingerprint),
            Some(diff_fingerprint(&file))
        );

        file.hunks[0].new_text = "let a = 3;".to_string();

        assert!(
            rendered_text(cache.render(&file)).contains("let a = 3;"),
            "the refreshed body must serve the updated hunk"
        );
        // The entry keeps its slot across a refresh, so a re-render is observed
        // through the content it serves and the fingerprint it records, never
        // through the address of the stored value (c-1).
        assert_eq!(
            cache
                .entries
                .get("crates/tui/src/app.rs")
                .map(|entry| entry.fingerprint),
            Some(diff_fingerprint(&file)),
            "the refreshed entry must record the new fingerprint"
        );
        assert_eq!(cache.entries.len(), 1);
    }

    /// A header states the lines the hunk spans on both sides, so a patch row
    /// names its coordinates instead of the position of the hunk inside the
    /// patch, and the section hint the patch carries follows them
    /// (c-diff-cache-git-headers).
    #[test]
    fn the_header_states_the_ranges_and_the_context_hint() {
        let mut file = file_with_hunk("crates/tui/src/main.rs");
        file.hunks[0].old_start = 12;
        file.hunks[0].old_lines = 3;
        file.hunks[0].new_start = 12;
        file.hunks[0].new_lines = 4;
        file.hunks[0].old_text = "let a = 1;\nlet b = 2;\nlet c = 3;".to_string();
        file.hunks[0].new_text = "let a = 1;\nlet b = 2;\nlet c = 4;\nlet d = 5;".to_string();
        file.hunks[0].context_hint = Some("fn main".to_string());

        let mut cache = DiffRenderCache::default();
        let text = rendered_text(cache.render(&file));

        assert!(text.contains("@@ -12,3 +12,4 @@ fn main"), "{text}");
    }

    /// A creation keeps the marker that names the operation beside the ranges it
    /// spans, so the card never hides what kind of change it audits
    /// (c-diff-cache-git-headers).
    #[test]
    fn a_created_file_keeps_its_marker_beside_the_ranges() {
        let mut file = file_with_hunk("crates/tui/src/new.rs");
        file.op_type = EditOp::Create;
        file.hunks[0].old_start = 0;
        file.hunks[0].old_lines = 0;
        file.hunks[0].new_start = 1;
        file.hunks[0].new_lines = 1;
        file.hunks[0].old_text = String::new();
        file.hunks[0].new_text = "fn added() {}".to_string();

        let mut cache = DiffRenderCache::default();
        let text = rendered_text(cache.render(&file));

        assert!(text.contains("@@ -0,0 +1,1 @@ new file"), "{text}");
    }

    /// A deletion mirrors the creation: it names the removed lines and keeps the
    /// marker of the operation (c-diff-cache-git-headers).
    #[test]
    fn a_deleted_file_keeps_its_marker_beside_the_ranges() {
        let mut file = file_with_hunk("crates/tui/src/old.rs");
        file.op_type = EditOp::Delete;
        file.hunks[0].old_start = 1;
        file.hunks[0].old_lines = 1;
        file.hunks[0].new_start = 0;
        file.hunks[0].new_lines = 0;
        file.hunks[0].old_text = "fn removed() {}".to_string();
        file.hunks[0].new_text = String::new();

        let mut cache = DiffRenderCache::default();
        let text = rendered_text(cache.render(&file));

        assert!(text.contains("@@ -1,1 +0,0 @@ deleted file"), "{text}");
    }

    /// Every rendered row records the lines it occupies on both sides, which is
    /// the only source a two-column gutter may read (c-two-column-gutter).
    #[test]
    fn the_recorded_row_numbers_follow_the_patch_tags() {
        let mut file = file_with_hunk("crates/tui/src/app.rs");
        file.hunks[0].old_start = 10;
        file.hunks[0].old_lines = 2;
        file.hunks[0].new_start = 10;
        file.hunks[0].new_lines = 2;
        file.hunks[0].old_text = "let a = 1;\nlet b = 2;".to_string();
        file.hunks[0].new_text = "let a = 1;\nlet b = 3;".to_string();

        let mut cache = DiffRenderCache::default();
        let rendered = cache.render(&file);

        let coordinates: Vec<(Option<usize>, Option<usize>)> = rendered
            .row_numbers
            .iter()
            .map(|numbers| (numbers.old, numbers.new))
            .collect();

        assert_eq!(rendered.row_numbers.len(), rendered.lines.len());
        assert_eq!(
            coordinates,
            vec![
                (None, None),
                (Some(10), Some(10)),
                (Some(11), None),
                (None, Some(11)),
            ],
            "a header carries no number, context keeps both sides, a removal \
             keeps the source line, an addition keeps the result line"
        );
    }

    /// A hunk that did not state its coordinates leaves the gutter without
    /// numbers instead of numbering from zero (c-two-column-gutter).
    #[test]
    fn an_unstated_coordinate_carries_no_number() {
        let mut file = file_with_hunk("crates/tui/src/app.rs");
        file.hunks[0].old_start = 0;
        file.hunks[0].old_lines = 0;
        file.hunks[0].new_start = 0;
        file.hunks[0].new_lines = 0;

        let mut cache = DiffRenderCache::default();
        let rendered = cache.render(&file);

        assert!(
            rendered
                .row_numbers
                .iter()
                .all(|numbers| numbers.old.is_none() && numbers.new.is_none()),
            "an unknown coordinate must never turn into a number"
        );
    }

    /// Concatenated text of every rendered diff row.
    fn rendered_text(rendered: &RenderedDiff) -> String {
        rendered
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }
}
