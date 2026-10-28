use std::collections::HashMap;

use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::theme::engine::current_engine;

use super::state::{EditOp, StreamingFileEdit};

/// Cache of rendered diff lines for expanded file edits.
///
/// Computing a `similar::TextDiff` is comparatively expensive and was
/// previously repeated for every expanded file on every frame and navigation
/// key. Entries are keyed by file path and invalidated by a cheap fingerprint
/// of the file's hunks, so a changed hunk always re-renders.
#[derive(Default)]
pub struct DiffRenderCache {
    entries: HashMap<String, DiffCacheEntry>,
}

struct DiffCacheEntry {
    fingerprint: u64,
    rendered: RenderedDiff,
}

/// A rendered diff body together with the row offsets of its hunk headers.
///
/// The offsets are relative to `lines`, so the Viewport projects them through
/// its own visual coordinates and names the hunk under the top edge without
/// re-deriving the patch layout (c-1).
#[derive(Debug, Clone, Default)]
pub(crate) struct RenderedDiff {
    pub lines: Vec<Line<'static>>,
    pub hunk_rows: Vec<usize>,
}

impl DiffRenderCache {
    /// Returns the rendered diff body for `file`, recomputing it only when the
    /// cached fingerprint no longer matches the file's hunks.
    pub(super) fn lines_for(&mut self, file: &StreamingFileEdit) -> Vec<Line<'static>> {
        self.render(file).lines
    }

    /// Renders the diff body together with the row offsets of its hunk headers.
    ///
    /// The Viewport needs those offsets to name the hunk the top edge sits
    /// inside, so the overlay reuses the very rows that were drawn instead of
    /// re-deriving them from the patch text (c-1).
    pub(crate) fn render(&mut self, file: &StreamingFileEdit) -> RenderedDiff {
        let fingerprint = diff_fingerprint(file);
        if let Some(rendered) = self.cached_render(&file.path, fingerprint) {
            return rendered;
        }
        let rendered = render_expanded_diff(file);
        self.entries.insert(
            file.path.clone(),
            DiffCacheEntry {
                fingerprint,
                rendered: rendered.clone(),
            },
        );
        rendered
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

    fn cached_render(&self, path: &str, fingerprint: u64) -> Option<RenderedDiff> {
        let entry = self.entries.get(path)?;
        if entry.fingerprint == fingerprint {
            Some(entry.rendered.clone())
        } else {
            None
        }
    }
}

/// Cheap content fingerprint of a file's hunks, used to invalidate the diff
/// render cache without recomputing the diff itself.
fn diff_fingerprint(file: &StreamingFileEdit) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    file.path.hash(&mut hasher);
    (file.op_type as u8).hash(&mut hasher);
    for hunk in &file.hunks {
        hunk.hunk_index.hash(&mut hasher);
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
    let palette = engine.palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut hunk_rows: Vec<usize> = Vec::new();
    for hunk in &file.hunks {
        hunk_rows.push(lines.len());
        if file.op_type == EditOp::Create {
            lines.push(Line::from(vec![Span::styled(
                "  @@ new file @@",
                engine.fg(palette.diff_added).bold(),
            )]));
            for line in hunk.new_text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  + ", engine.fg(palette.diff_added).bold()),
                    Span::styled(line.to_string(), engine.fg(palette.diff_added)),
                ]));
            }
        } else if file.op_type == EditOp::Delete {
            lines.push(Line::from(vec![Span::styled(
                "  @@ deleted file @@",
                engine.fg(palette.diff_removed).bold(),
            )]));
            for line in hunk.old_text.lines() {
                lines.push(Line::from(vec![
                    Span::styled("  - ", engine.fg(palette.diff_removed).bold()),
                    Span::styled(line.to_string(), engine.fg(palette.diff_removed)),
                ]));
            }
        } else {
            let diff = similar::TextDiff::from_lines(&hunk.old_text, &hunk.new_text);
            let header_text = if file.hunks.len() > 1 {
                format!("  @@ hunk {} @@", hunk.hunk_index + 1)
            } else {
                "  @@ diff @@".to_string()
            };
            lines.push(Line::from(vec![Span::styled(
                header_text,
                engine.fg(palette.status_info).bold(),
            )]));

            for op in diff.ops() {
                for change in diff.iter_changes(op) {
                    let line_str = change.value().trim_end_matches(['\r', '\n']);
                    match change.tag() {
                        similar::ChangeTag::Delete => {
                            lines.push(Line::from(vec![
                                Span::styled("  - ", engine.fg(palette.diff_removed).bold()),
                                Span::styled(line_str.to_string(), engine.fg(palette.diff_removed)),
                            ]));
                        }
                        similar::ChangeTag::Insert => {
                            lines.push(Line::from(vec![
                                Span::styled("  + ", engine.fg(palette.diff_added).bold()),
                                Span::styled(line_str.to_string(), engine.fg(palette.diff_added)),
                            ]));
                        }
                        similar::ChangeTag::Equal => {
                            lines.push(Line::from(vec![
                                Span::styled("    ", engine.fg(palette.diff_context)),
                                Span::styled(line_str.to_string(), engine.fg(palette.diff_context)),
                            ]));
                        }
                    }
                }
            }
        }
    }
    RenderedDiff { lines, hunk_rows }
}

#[cfg(test)]
mod invalidate_tests {
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
                old_text: "let a = 1;".to_string(),
                new_text: "let a = 2;".to_string(),
            }],
            expanded: true,
            retry_info: None,
        }
    }

    #[test]
    fn invalidate_drops_only_the_restarted_path() {
        let mut cache = DiffRenderCache::default();
        let kept = file_with_hunk("crates/tui/src/main.rs");
        let retried = file_with_hunk("crates/tui/src/app.rs");

        assert!(!cache.lines_for(&kept).is_empty());
        assert!(!cache.lines_for(&retried).is_empty());
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
}
