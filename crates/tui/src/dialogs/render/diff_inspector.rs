//! Fullscreen two-pane audit of the change set the Develop viewport stages.
//!
//! The card claims almost the whole terminal and reuses the shared modal
//! formula, so it breathes exactly like every other dialog (Conventions §6.7,
//! §6.8). The left pane walks the audited files with their operation badge and
//! verification status; the right pane reads the diff of the highlighted file in
//! isolation, with a line-number gutter and its own scroll offset, so a long
//! patch is never interrupted by the file list (c-3).
//!
//! The audited set is the live staged list, never a frozen copy: it is refreshed
//! by every edit-stream event, so a file that is still streaming reads the same
//! here as in the Viewport, and the file the stream currently writes carries an
//! animated spinner beside its running badge (c-3). The diff body comes from the
//! `DiffRenderCache` the Develop viewport already fills, so the audit paints
//! exactly the rows the REPL laid out and a repeated frame never recomputes a
//! `similar::TextDiff` (c-1, c-3). Every visible phrase is resolved through the
//! runtime locale (`crate::i18n::active_t`), so a hot-swapped locale reaches the
//! inspector without touching the renderer (Conventions §9, c-4).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::diff_inspector_geometry;
use crate::app::DiffInspectorState;
use crate::components::modal::{
    render_dialog_footer, render_modal_container, verb_hint, ModalTone,
};
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::header::truncate_to_width;
use crate::views::repl::render::{file_op_badge, SPINNER_FRAMES};
use crate::views::repl::{DiffRenderCache, FileEditStatus, StreamingFileEdit};

/// Renders the inspector card over the Develop viewport.
///
/// `cache` is the diff render cache the Develop viewport owns. Sharing it keeps
/// one rendered patch per file for both surfaces, so the audit and the REPL can
/// never disagree about a diff body (c-1, c-3).
///
/// `spinner_frame` is the animation frame the running turn advanced, so the file
/// the stream currently writes carries the same spinner here as it does in the
/// Viewport (c-3).
pub(crate) fn render_diff_inspector(
    frame: &mut ratatui::Frame,
    inspector: &mut DiffInspectorState,
    cache: &mut DiffRenderCache,
    spinner_frame: usize,
) {
    let engine = current_engine();
    let geometry = diff_inspector_geometry(frame.area());
    render_modal_container(
        frame,
        geometry.area,
        &active_t("dialog.diff_inspector.title"),
        ModalTone::Info,
    );

    // The audited set is refreshed by every edit-stream event, so it can grow and
    // shrink while the audit is open; the highlight still has to name a real file
    // (c-3).
    if inspector.selected_file >= inspector.files.len() {
        inspector.selected_file = inspector.files.len().saturating_sub(1);
    }

    let spinner = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
    render_file_pane(frame, inspector, &engine, geometry.files, spinner);
    let (hunk_total, hunk_active) =
        render_diff_pane(frame, inspector, cache, &engine, geometry.diff);
    render_action_bar(
        frame,
        inspector,
        hunk_total,
        hunk_active,
        &engine,
        geometry.action_bar,
    );
    render_dialog_footer(
        frame,
        geometry.hint_row,
        &verb_hint(active_t("dialog.diff_inspector.footer")),
    );
}

/// Renders the left pane: the audited files in Viewport order.
fn render_file_pane(
    frame: &mut ratatui::Frame,
    inspector: &DiffInspectorState,
    engine: &ThemeEngine,
    area: Rect,
    spinner: &str,
) {
    let palette = engine.palette();
    // Rows are laid out for the exact column budget of the pane, so a deep
    // repository path truncates instead of pushing the verification badge out
    // of the pane (c-3).
    let inner_width = usize::from(area.width.saturating_sub(2));
    let rows = file_rows(inspector, engine, inner_width, spinner);
    // The highlight stays visible: the window follows the selected row the way
    // the file list of every other picker does (Conventions §6.2).
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let offset = (inspector.selected_file + 1).saturating_sub(visible);
    let title = active_t("dialog.diff_inspector.files_title").replacen(
        "{}",
        &inspector.files.len().to_string(),
        1,
    );
    let block = Block::default()
        .title(Line::from(Span::styled(
            title,
            pane_title_style(engine, inspector.focus.is_files()),
        )))
        .borders(Borders::ALL)
        .border_style(pane_border_style(engine, inspector.focus.is_files()))
        .style(Style::default().bg(engine.color(palette.surface)));
    let paragraph = Paragraph::new(rows)
        .block(block)
        .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0));
    frame.render_widget(paragraph, area);
}

/// Border style of a pane: the focused pane carries the accent role, while the
/// idle one recedes into the plain border role (Conventions §8.4, c-2).
fn pane_border_style(engine: &ThemeEngine, focused: bool) -> Style {
    let palette = engine.palette();
    if focused {
        engine.fg(palette.status_info)
    } else {
        engine.fg(palette.border)
    }
}

/// Title style of a pane, matching the accent of its border so the pane that
/// owns the keyboard is readable without a textual marker.
fn pane_title_style(engine: &ThemeEngine, focused: bool) -> Style {
    let palette = engine.palette();
    if focused {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.text_muted)
    }
}

/// Builds one row per audited file: selection marker, operation badge, path,
/// patch count, and verification status, fitted to `width` columns.
///
/// The operation and verification badges are reserved before the path, so a
/// deep repository path truncates instead of hiding the status of the file, and
/// the patch count is dropped only when the row has no columns left (c-3). A
/// file the stream is still writing carries the animation frame beside its
/// badge, so the audit names the file being edited right now (c-3).
fn file_rows(
    inspector: &DiffInspectorState,
    engine: &ThemeEngine,
    width: usize,
    spinner: &str,
) -> Vec<Line<'static>> {
    let palette = engine.palette();
    if inspector.files.is_empty() {
        return vec![placeholder_line(
            engine,
            &active_t("dialog.diff_inspector.no_files"),
        )];
    }

    inspector
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let selected = index == inspector.selected_file;
            let marker = if selected { "▸ " } else { "  " };
            let op_badge = file_op_badge(engine, &file.op_type);
            let status = status_badge(engine, file.status, spinner);

            // Columns the marker, the operation badge, its trailing space, and
            // the last space plus the status badge already claim.
            let head_width = 2 + op_badge.width() + 1;
            let tail_width = 1 + status.width();
            let path_budget = width.saturating_sub(head_width.saturating_add(tail_width));
            let path = if path_budget == 0 {
                String::new()
            } else {
                truncate_to_width(&file.path, path_budget)
            };
            let path_width = Span::raw(path.as_str()).width();

            let patch = patch_count(file);
            let patch_width = 1 + Span::raw(patch.as_str()).width();
            let show_patch =
                width.saturating_sub(head_width + path_width + tail_width) >= patch_width;

            let mut spans = vec![
                Span::styled(marker, engine.fg(palette.status_info)),
                op_badge,
                Span::raw(" "),
                Span::styled(path, Style::default().bold()),
            ];
            if show_patch {
                spans.push(Span::styled(
                    format!(" {}", patch),
                    engine.fg(palette.text_muted),
                ));
            }
            spans.push(Span::raw(" "));
            spans.push(status);

            let mut line = Line::from(spans);
            if selected {
                line = line.style(engine.bg(palette.overlay).bold());
            }
            line
        })
        .collect()
}

/// Counts the patches the audited set applied to one file through the catalog
/// label the Develop viewport already uses, so both surfaces describe a file
/// identically (c-4).
fn patch_count(file: &StreamingFileEdit) -> String {
    active_t("repl.hunk_count").replacen("{}", &file.hunks.len().to_string(), 1)
}

/// Resolves the verification status badge of one audited file.
///
/// A file the stream is still writing carries the animation frame beside its
/// badge, so the audit names the file being edited right now instead of leaving
/// a static status that could belong to any row (c-3). A settled file renders
/// its badge alone.
fn status_badge(engine: &ThemeEngine, status: FileEditStatus, spinner: &str) -> Span<'static> {
    let palette = engine.palette();
    let (key, color) = match status {
        FileEditStatus::Ok => ("dialog.diff_inspector.badge_ok", palette.status_success),
        FileEditStatus::Error => ("dialog.diff_inspector.badge_error", palette.status_error),
        FileEditStatus::Running => (
            "dialog.diff_inspector.badge_running",
            palette.status_warning,
        ),
        FileEditStatus::Retrying => (
            "dialog.diff_inspector.badge_retrying",
            palette.status_warning,
        ),
    };
    let label = active_t(key);
    if matches!(status, FileEditStatus::Running | FileEditStatus::Retrying) {
        Span::styled(format!("{spinner} {label}"), engine.fg(color).bold())
    } else {
        Span::styled(label, engine.fg(color).bold())
    }
}

/// Renders the right pane: the isolated diff read area of the highlighted file.
///
/// It returns the hunk count of the rendered patch together with the one-based
/// number of the hunk under the top edge, so the action bar names the same
/// geometry the pane just drew (c-1).
fn render_diff_pane(
    frame: &mut ratatui::Frame,
    inspector: &mut DiffInspectorState,
    cache: &mut DiffRenderCache,
    engine: &ThemeEngine,
    area: Rect,
) -> (usize, Option<usize>) {
    let palette = engine.palette();
    let mut hunk_rows: Vec<usize> = Vec::new();
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(file) = inspector.files.get(inspector.selected_file) {
        let rendered = cache.render(file);
        if rendered.lines.is_empty() {
            lines.push(placeholder_line(
                engine,
                &active_t("dialog.diff_inspector.no_diff"),
            ));
        } else {
            hunk_rows = rendered.hunk_rows;
            lines = numbered_diff_lines(&rendered.lines, engine);
        }
    } else {
        lines.push(placeholder_line(
            engine,
            &active_t("dialog.diff_inspector.no_diff"),
        ));
    }

    // The read area owns its own offset: the file list and the diff pane never
    // share a scroll position, so reading a long patch never moves the index
    // (c-3). An offset left over from a previously highlighted file is clamped
    // to the body that is actually drawn.
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let max_scroll = lines.len().saturating_sub(visible);
    inspector.diff_scroll = inspector
        .diff_scroll
        .min(u16::try_from(max_scroll).unwrap_or(u16::MAX));

    let block = Block::default()
        .title(Line::from(Span::styled(
            active_t("dialog.diff_inspector.diff_title"),
            pane_title_style(engine, inspector.focus.is_diff()),
        )))
        .borders(Borders::ALL)
        .border_style(pane_border_style(engine, inspector.focus.is_diff()))
        .style(Style::default().bg(engine.color(palette.surface)));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((inspector.diff_scroll, 0));
    frame.render_widget(paragraph, area);

    (
        hunk_rows.len(),
        active_hunk(&hunk_rows, inspector.diff_scroll),
    )
}

/// Prepends the line-number gutter to a cached diff body.
///
/// The rows themselves are reused unchanged, so the audit reads exactly the
/// patch the cache rendered and the gutter never re-derives a diff (c-3).
fn numbered_diff_lines(lines: &[Line<'static>], engine: &ThemeEngine) -> Vec<Line<'static>> {
    let number_style = engine.fg(engine.palette().text_muted);
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(format!("{:>4} ", index + 1), number_style));
            spans.extend(line.spans.iter().cloned());
            let mut numbered = Line::from(spans);
            numbered.style = line.style;
            numbered
        })
        .collect()
}

/// One-based number of the hunk whose header is the last one at or above the
/// top edge of the read area.
fn active_hunk(hunk_rows: &[usize], scroll: u16) -> Option<usize> {
    let top = usize::from(scroll);
    hunk_rows
        .iter()
        .enumerate()
        .filter_map(|(position, row)| (*row <= top).then_some(position + 1))
        .next_back()
}

/// Renders the action row: the highlighted file position and the hunk under the
/// top edge of the read area.
fn render_action_bar(
    frame: &mut ratatui::Frame,
    inspector: &DiffInspectorState,
    hunk_total: usize,
    hunk_active: Option<usize>,
    engine: &ThemeEngine,
    area: Rect,
) {
    let palette = engine.palette();
    let file_total = inspector.files.len();
    let file_position = if file_total == 0 {
        0
    } else {
        inspector.selected_file + 1
    };

    let mut spans = vec![Span::styled(
        progress_label(
            "dialog.diff_inspector.file_progress",
            file_position,
            file_total,
        ),
        engine.fg(palette.status_info).bold(),
    )];
    if hunk_total > 0 {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            progress_label(
                "dialog.diff_inspector.hunk_progress",
                hunk_active.unwrap_or(1),
                hunk_total,
            ),
            engine.fg(palette.text_secondary),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans)).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// Fills the two placeholders of a `first/second` catalog template.
fn progress_label(key: &str, first: usize, second: usize) -> String {
    active_t(key)
        .replacen("{}", &first.to_string(), 1)
        .replacen("{}", &second.to_string(), 1)
}

/// Builds a muted placeholder row for an empty pane.
fn placeholder_line(engine: &ThemeEngine, text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {}", text),
        engine.fg(engine.palette().text_muted),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DiffInspectorFocus;
    use crate::views::repl::{EditOp, StreamingHunk};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn file(
        path: &str,
        op: EditOp,
        status: FileEditStatus,
        patch: (&str, &str),
    ) -> StreamingFileEdit {
        StreamingFileEdit {
            path: path.to_string(),
            op_type: op,
            status,
            error: None,
            hunks: vec![StreamingHunk {
                hunk_index: 0,
                old_text: patch.0.to_string(),
                new_text: patch.1.to_string(),
            }],
            retry_info: None,
        }
    }

    /// Draws one inspector frame and flattens the whole grid into text.
    fn rendered(inspector: &mut DiffInspectorState, cache: &mut DiffRenderCache) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).expect("test backend");
        terminal
            .draw(|frame| render_diff_inspector(frame, inspector, cache, 0))
            .expect("the inspector frame draws");
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn file_list_shows_operation_and_verification_badges() {
        let mut inspector = DiffInspectorState {
            files: vec![
                file(
                    "crates/tui/src/app.rs",
                    EditOp::Replace,
                    FileEditStatus::Ok,
                    ("let a = 1;", "let a = 2;"),
                ),
                file(
                    "crates/tui/src/new.rs",
                    EditOp::Create,
                    FileEditStatus::Error,
                    ("", "fn added() {}"),
                ),
            ],
            selected_file: 0,
            diff_scroll: 0,
            focus: DiffInspectorFocus::Files,
        };
        let mut cache = DiffRenderCache::default();

        let text = rendered(&mut inspector, &mut cache);

        assert!(text.contains("[EDIT]"), "{text}");
        assert!(text.contains("[NEW]"), "{text}");
        assert!(text.contains("[OK]"), "{text}");
        assert!(text.contains("[ERR]"), "{text}");
        assert!(text.contains("crates/tui/src/app.rs"), "{text}");
        assert!(text.contains("crates/tui/src/new.rs"), "{text}");
    }

    #[test]
    fn read_area_numbers_every_rendered_diff_row() {
        let mut inspector = DiffInspectorState {
            files: vec![file(
                "crates/tui/src/app.rs",
                EditOp::Replace,
                FileEditStatus::Ok,
                ("let a = 1;", "let a = 2;"),
            )],
            selected_file: 0,
            diff_scroll: 0,
            focus: DiffInspectorFocus::Files,
        };
        let mut cache = DiffRenderCache::default();

        let text = rendered(&mut inspector, &mut cache);

        assert!(text.contains("   1   @@ diff @@"), "{text}");
        assert!(text.contains("   2   - let a = 1;"), "{text}");
        assert!(text.contains("   3   + let a = 2;"), "{text}");
    }

    #[test]
    fn scroll_offset_is_clamped_and_the_action_bar_names_the_progress() {
        let mut inspector = DiffInspectorState {
            files: vec![
                file(
                    "crates/tui/src/app.rs",
                    EditOp::Replace,
                    FileEditStatus::Ok,
                    ("let a = 1;", "let a = 2;"),
                ),
                file(
                    "crates/tui/src/main.rs",
                    EditOp::Replace,
                    FileEditStatus::Ok,
                    ("let b = 1;", "let b = 2;"),
                ),
            ],
            selected_file: 0,
            diff_scroll: u16::MAX,
            focus: DiffInspectorFocus::Files,
        };
        let mut cache = DiffRenderCache::default();

        let text = rendered(&mut inspector, &mut cache);

        assert!(
            inspector.diff_scroll < u16::MAX,
            "a stale offset is clamped to the drawn body"
        );
        assert!(text.contains("File 1/2"), "{text}");
        assert!(text.contains("Hunk 1/1"), "{text}");
    }
    /// The active pane is accented through the semantic roles and the idle one
    /// recedes, so which pane owns the vertical motion is readable without a
    /// textual marker (Conventions §8.4, c-2).
    #[test]
    fn pane_styles_follow_the_focus() {
        let engine = current_engine();

        assert_ne!(
            pane_border_style(&engine, true).fg,
            pane_border_style(&engine, false).fg,
            "the focused pane carries a distinct border role"
        );
        assert_ne!(
            pane_title_style(&engine, true).fg,
            pane_title_style(&engine, false).fg,
            "the focused title carries a distinct text role"
        );
    }
    /// A file the stream is still writing carries the animation frame beside its
    /// badge, so the audit names the file being edited right now (c-3), while a
    /// settled file renders its status alone.
    #[test]
    fn running_file_carries_the_spinner_and_the_run_badge() {
        let mut inspector = DiffInspectorState {
            files: vec![
                file(
                    "crates/tui/src/streaming.rs",
                    EditOp::Replace,
                    FileEditStatus::Running,
                    ("let a = 1;", "let a = 2;"),
                ),
                file(
                    "crates/tui/src/settled.rs",
                    EditOp::Replace,
                    FileEditStatus::Ok,
                    ("let b = 1;", "let b = 2;"),
                ),
            ],
            selected_file: 0,
            diff_scroll: 0,
            focus: DiffInspectorFocus::Files,
        };
        let mut cache = DiffRenderCache::default();

        let text = rendered(&mut inspector, &mut cache);

        let spinner = SPINNER_FRAMES[0];
        assert!(text.contains(&format!("{spinner} [RUN]")), "{text}");
        assert!(text.contains("[OK]"), "{text}");
        assert!(
            !text.contains(&format!("{spinner} [OK]")),
            "a settled file never keeps the animation frame: {text}"
        );
    }
}
