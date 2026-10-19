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
    lines: Vec<Line<'static>>,
}

impl DiffRenderCache {
    /// Returns the rendered diff body for `file`, recomputing it only when the
    /// cached fingerprint no longer matches the file's hunks.
    pub(super) fn lines_for(&mut self, file: &StreamingFileEdit) -> Vec<Line<'static>> {
        let fingerprint = diff_fingerprint(file);
        if let Some(lines) = self.cached_lines(&file.path, fingerprint) {
            return lines;
        }
        let lines = render_expanded_diff_lines(file);
        self.entries.insert(
            file.path.clone(),
            DiffCacheEntry {
                fingerprint,
                lines: lines.clone(),
            },
        );
        lines
    }

    fn cached_lines(&self, path: &str, fingerprint: u64) -> Option<Vec<Line<'static>>> {
        let entry = self.entries.get(path)?;
        if entry.fingerprint == fingerprint {
            Some(entry.lines.clone())
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
fn render_expanded_diff_lines(file: &StreamingFileEdit) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for hunk in &file.hunks {
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
    lines
}
