use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use super::editor_lines;
use crate::app::PATH_COMPLETION_MAX_ROWS;
use crate::components::modal::{render_dialog_footer, verb_hint};
use crate::config::PrimaryModifier;
use crate::i18n::active_t;
use crate::theme::engine::{current_engine, ThemeEngine};
use crate::ui::centered_rect;

/// Width of the review launch dialog, as a percentage of the terminal width.
pub const REVIEW_DIALOG_WIDTH: u16 = 64;
/// Height of the review launch dialog, as a percentage of the terminal height.
///
///
/// The card stacks its heterogeneous zones — header, reviewer list, the
/// review-preset section, the target field, and the instructions editor — with
/// one blank row between each of them, plus the action bar and its docked hint
/// strip. The target field belongs to the custom audit alone, so that launch
/// mode is the taller shape (c-5). It is therefore taller than a plain form
/// dialog so the air never costs a section (Conventions §6.7).
pub const REVIEW_DIALOG_HEIGHT: u16 = 88;
/// Rows of the target-paths list the dialog draws at once.
///
/// The list grows with every `C-j` row, so the block is capped: a wider scope is
/// reached by walking the rows, which keeps the instructions editor and the
/// action bar on the card (c-3).
const REVIEW_DIALOG_PATH_MAX_ROWS: usize = 4;

/// Height of the target-paths block for `rows` path rows: one row per path plus
/// the row of the live match counter, framed by the block borders, so the block
/// is exactly as tall as the rows it frames plus its own feedback (c-3, c-6).
fn review_dialog_path_height(rows: usize) -> u16 {
    let rows = rows.clamp(1, REVIEW_DIALOG_PATH_MAX_ROWS) as u16;
    super::PICKER_DIALOG_PATH_HEIGHT + rows
}
/// Number of review-preset rows the section shows at once; the rest stay
/// reachable through the shared list arithmetic, so the card never grows with
/// the number of declared presets (c-5).
const REVIEW_DIALOG_PRESET_MAX_ROWS: u16 = 3;
/// Full drawn confirm-button label, resolved from the runtime locale.
///
/// Rendering and mouse hit-testing both measure this label, so the clickable
/// button can never drift from the drawn one (Conventions §6, §9, c-1).
fn review_confirm_label() -> String {
    format!(" [ {} ] ", active_t("modal.start_review"))
}

/// Full drawn cancel-button label, resolved from the runtime locale.
fn review_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Height of the header block (summary and target metadata).
const REVIEW_HEADER_HEIGHT: u16 = 6;

/// Rectangles of the review dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the reviewer model list (one row per entry).
    pub model_rows: Rect,
    /// Inner rows of the review-preset section, zero-height when the project
    /// declares no preset (c-5).
    pub preset_rows: Rect,
    /// The target-paths input block; zero-height under a highlighted preset,
    /// which supplies its own audited paths (c-5).
    pub path_area: Rect,
    /// Inner rows of the multi-row target-paths list, above its match counter.
    pub path_rows: Rect,
    /// Contextual directory completion menu of the active row; zero-height when
    /// the row offers no candidate (c-5).
    pub completions: Rect,
    /// The instruction editor block.
    pub editor_area: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Computes the dialog frame, the reviewer-row area, the review-preset rows,
/// and the action buttons from the live dialog state and the same layout the
/// renderer draws with.
///
/// The launch mode governs the optional section (c-5): a highlighted preset
/// owns the launch, so the target field — the input aid of the custom audit —
/// collapses to zero rows, while the preset section itself stays visible as the
/// mode switcher. An empty preset list collapses its section the same way.
pub fn review_dialog_geometry(
    term: Rect,
    dialog: &crate::app::ReviewDialogState,
) -> ReviewDialogGeometry {
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
    let (chunks, preset_section) = review_dialog_chunks(dialog, area);
    review_dialog_geometry_from(
        area,
        chunks,
        preset_section,
        dialog.visible_completion_rows(),
    )
}

/// Splits the review dialog card into the shared picker chunks and the optional
/// preset section, applying the launch mode to the target field.
///
/// The renderer and every hit-tester go through this construction, so the
/// drawn rows, the clickable rows, and the launch mode can never disagree
/// (Conventions §6.6, §10).
fn review_dialog_chunks(
    dialog: &crate::app::ReviewDialogState,
    area: Rect,
) -> (super::PickerDialogChunks, Rect) {
    // A highlighted preset owns the launch and supplies its own audited paths,
    // so the target field — the input aid of the custom audit — leaves the card
    // entirely; the preset section itself is the mode switcher and stays in
    // both modes (c-5).
    let path_height = if dialog.is_custom_launch() {
        review_dialog_path_height(dialog.path_row_count())
    } else {
        0
    };
    super::picker_dialog_chunks_with_presets(
        area,
        dialog.selector.len(),
        REVIEW_HEADER_HEIGHT,
        path_height,
        preset_section_rows(dialog.preset_selector.item_count()),
    )
}

/// Builds the dialog rectangles from the shared picker layout and the optional
/// preset section that layout produced.
///
/// The renderer and the mouse hit-tester both go through this construction, so
/// the drawn rows and the clickable rows are literally the same rectangles
/// (Conventions §6.6, §10).
fn review_dialog_geometry_from(
    area: Rect,
    chunks: super::PickerDialogChunks,
    preset_section: Rect,
    completion_rows: usize,
) -> ReviewDialogGeometry {
    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) = super::picker_button_rects(
        chunks.buttons,
        &review_confirm_label(),
        &review_cancel_label(),
    );

    // The list keeps the last inner row of the block for the live match counter,
    // so the rows and the counter can never overlap on a squeezed card.
    let path_inner = super::inset(chunks.path);
    let path_rows = Rect::new(
        path_inner.x,
        path_inner.y,
        path_inner.width,
        path_inner.height.saturating_sub(1),
    );

    ReviewDialogGeometry {
        area,
        model_rows: super::inset(chunks.list),
        preset_rows: super::inset(preset_section),
        path_area: chunks.path,
        path_rows,
        completions: completion_area(&chunks, completion_rows),
        editor_area: chunks.editor,
        confirm_button,
        cancel_button,
    }
}

/// Rectangles of the contextual directory completion menu (c-5).
///
/// The menu drops from the bottom edge of the target-paths block and grows
/// downward, one row per candidate plus its own borders, and never covers the
/// action bar: a long candidate list can never hide the buttons the dialog is
/// waiting for.
fn completion_area(chunks: &super::PickerDialogChunks, completion_rows: usize) -> Rect {
    if completion_rows == 0 || chunks.path.height == 0 {
        return Rect::new(chunks.path.x, chunks.path.y, 0, 0);
    }
    let rows = completion_rows.min(PATH_COMPLETION_MAX_ROWS) as u16;
    let height = rows + 2;
    let limit = chunks.buttons.y;
    let y = chunks.path.bottom().min(limit.saturating_sub(height));
    Rect::new(chunks.path.x, y, chunks.path.width, height)
}

/// Visible preset rows the section draws for `preset_count` rows.
///
/// The section is capped so the card always keeps room for the target field and
/// the instructions editor; a project without presets collapses it to zero rows
/// (c-5).
fn preset_section_rows(preset_count: usize) -> usize {
    preset_count.min(REVIEW_DIALOG_PRESET_MAX_ROWS as usize)
}
/// Draws one target-path row, filling the active row across the whole inner
/// width of the block.
///
/// The active row is the one that receives typing, so it reads as the selected
/// row of the list exactly like the highlighted row of the preset section and
/// of the directory completion menu. The band is painted by padding the row to
/// the column budget and patching the line style, so it covers the empty cells
/// of the row instead of only the typed characters (Conventions §8.3, c-3).
fn path_row_line(
    spans: Vec<Span<'static>>,
    active: bool,
    width: usize,
    engine: &ThemeEngine,
) -> Line<'static> {
    let mut line = Line::from(spans);
    if !active || width == 0 {
        return line;
    }
    let used: usize = line.spans.iter().map(|span| span.content.width()).sum();
    line.spans
        .push(Span::raw(" ".repeat(width.saturating_sub(used))));
    line.style(engine.bg(engine.palette().overlay))
}
pub fn render_review_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::ReviewDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let entry_count = dialog.selector.len();
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", active_t("dialog.review_launch.title")))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.button_active_bg))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let (chunks, preset_section) = review_dialog_chunks(dialog, area);
    let geometry = review_dialog_geometry_from(
        area,
        chunks,
        preset_section,
        dialog.visible_completion_rows(),
    );
    let super::PickerDialogChunks {
        header: header_area,
        list: list_area,
        path: path_area,
        editor: editor_area,
        buttons: buttons_area,
        footer: footer_area,
    } = chunks;

    let dim = engine.fg(palette.text_muted);

    // Header: what is about to be reviewed. The note names the audited set of
    // the active launch mode: the custom audit narrows the User Context with a
    // target pattern, while a highlighted preset supplies its own groups and
    // paths (c-5).
    let note_key = if dialog.is_custom_launch() {
        "dialog.review_launch.note"
    } else {
        "dialog.review_launch.note_preset"
    };
    let header_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.review_launch.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(active_t("dialog.review_launch.files_label"), dim),
            Span::styled(
                dialog.files_count.to_string(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(active_t("dialog.review_launch.tokens_label"), dim),
            Span::styled(
                format!("~{}", dialog.estimated_tokens),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::from(Span::styled(active_t(note_key), dim)),
    ];
    frame.render_widget(
        Paragraph::new(header_lines).wrap(Wrap { trim: false }),
        header_area,
    );

    // Reviewer list. The shared `ModelSelector` owns the rows, numbering,
    // scrolling, and empty-list safety; the block adds the framing and the
    // focus color that shows which section owns the keyboard
    // (c-embedded-model-list, c-unified-dialog-integration).
    let list_title = if entry_count > 1 {
        active_t("dialog.review_launch.model_list_counter")
            .replacen("{}", &(dialog.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.review_launch.model_list_title")
    };
    let list_border = if dialog.focus.is_model_list() {
        engine.fg(palette.button_active_bg).bold()
    } else {
        engine.fg(palette.border)
    };
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(list_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    if entry_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                active_t("dialog.review_launch.no_models"),
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = dialog
            .selector
            .render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Review presets: a selectable section that launches the groups the project
    // declares. Row zero is the explicit `Custom` entry, so a manual launch is
    // always one row away after a preset has been inspected (c-5). The rows are
    // drawn into the rectangles the mouse hit-tests, because both come from one
    // geometry construction. The section doubles as the mode switcher: it stays
    // visible under a highlighted preset, while the preset's own launch removes
    // the custom input aids below it.
    if geometry.preset_rows.height > 0 {
        let preset_border = if dialog.focus.is_preset_list() {
            engine.fg(palette.button_active_bg).bold()
        } else {
            engine.fg(palette.border)
        };
        let preset_block = Block::default()
            .title(active_t("dialog.review_launch.presets_title"))
            .borders(Borders::ALL)
            .border_style(preset_border)
            .style(Style::default().bg(engine.color(palette.surface)));
        frame.render_widget(preset_block, preset_section);

        let labels = dialog.preset_labels();
        let visible_rows = geometry.preset_rows.height as usize;
        let start = dialog.preset_selector.scroll_offset();
        let mut preset_lines = Vec::new();
        for (index, label) in labels.iter().enumerate().skip(start).take(visible_rows) {
            let is_selected = index == dialog.preset_selector.selected_index();
            let marker = if is_selected { " ▶ " } else { "   " };
            let marker_style = if is_selected {
                engine.fg(palette.status_success).bold()
            } else {
                engine.fg(palette.text_muted)
            };
            let row_style = if is_selected {
                engine
                    .bg(palette.overlay)
                    .fg(engine.color(palette.text_primary))
                    .bold()
            } else {
                engine.fg(palette.text_secondary)
            };
            preset_lines.push(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(label.clone(), row_style),
            ]));
        }
        frame.render_widget(Paragraph::new(preset_lines), geometry.preset_rows);
    }

    // Target paths field. A single-line input that narrows the review to a
    // directory or glob without touching the User Context layer: the value rides
    // along in `ReviewStartParams::path_patterns` and the server merges the
    // matched files with the developer's layer (c-review-context-input). The
    // block exists only in the custom audit, so a highlighted preset leaves no
    // empty frame behind (c-5). The cursor is drawn only when the field owns
    // focus.
    if path_area.height > 0 {
        let show_path_cursor = dialog.focus.is_path_editor();
        let path_border = if show_path_cursor {
            engine.fg(palette.status_info).bold()
        } else {
            engine.fg(palette.border)
        };
        let path_block = Block::default()
            .title(format!(" {} ", active_t("dialog.review_launch.path_title")))
            .borders(Borders::ALL)
            .border_style(path_border)
            .style(Style::default().bg(engine.color(palette.surface)));
        let path_inner = path_block.inner(path_area);
        frame.render_widget(path_block, path_area);

        // One input line per path row, with the cursor drawn only in the active
        // row and only while the section owns the keyboard. The window follows
        // the active row, so a scope longer than the block never hides the row
        // being typed, and every drawn row is padded into a full-width band so
        // the active one reads as the selected row of the list (c-3).
        let input_rows = geometry.path_rows.height as usize;
        let row_width = usize::from(geometry.path_rows.width);
        let start = dialog.path_visible_start(input_rows);
        let mut path_content = Vec::new();
        for (offset, editor) in dialog
            .path_inputs
            .iter()
            .skip(start)
            .take(input_rows)
            .enumerate()
        {
            let index = start + offset;
            let is_active = index == dialog.path_active;
            let row_cursor = show_path_cursor && is_active;
            if editor.is_empty() {
                let mut spans = vec![Span::raw("  > ")];
                if row_cursor {
                    spans.push(Span::styled("█", engine.fg(palette.status_warning)));
                }
                if index == 0 {
                    spans.push(Span::styled(
                        active_t("dialog.review_launch.path_placeholder"),
                        dim,
                    ));
                }
                path_content.push(path_row_line(spans, is_active, row_width, &engine));
            } else {
                for line in editor_lines(editor, row_cursor) {
                    let mut spans = vec![Span::raw("  > ")];
                    spans.extend(line.spans);
                    path_content.push(path_row_line(spans, is_active, row_width, &engine));
                }
            }
        }
        frame.render_widget(
            Paragraph::new(path_content).wrap(Wrap { trim: false }),
            geometry.path_rows,
        );

        // Live match counter: the entered rows are evaluated together against
        // the repository file list with the same `GlobPattern` the server
        // applies, so the number on screen is the number a launch would select.
        // An empty list narrows nothing; rows that select no file are an error
        // state that also blocks the launch (c-6).
        if path_inner.height > input_rows as u16 {
            let counter_row = Rect::new(
                path_inner.x,
                path_inner.y + input_rows as u16,
                path_inner.width,
                1,
            );
            let (counter_text, counter_style) = match dialog.pattern_match_count() {
                None => (active_t("dialog.review_launch.counter_idle"), dim),
                Some(0) => (
                    active_t("dialog.review_launch.counter_matches").replacen("{}", "0", 1),
                    engine.fg(palette.status_error).bold(),
                ),
                Some(matches) => (
                    active_t("dialog.review_launch.counter_matches").replacen(
                        "{}",
                        &matches.to_string(),
                        1,
                    ),
                    engine.fg(palette.status_success).bold(),
                ),
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(counter_text, counter_style))),
                counter_row,
            );
        }
    }
    // Instruction editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = dialog.focus.is_prompt_editor();
    let editor_title_style = if show_cursor {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let mut editor_text = vec![Line::from(Span::styled(
        active_t("dialog.review_launch.editor_title"),
        editor_title_style,
    ))];
    if dialog.prompt_editor.is_empty() {
        let mut spans = vec![Span::raw("  > ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.review_launch.editor_placeholder"),
            dim,
        ));
        editor_text.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.prompt_editor, show_cursor) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(line.spans);
            editor_text.push(Line::from(spans));
        }
    }
    frame.render_widget(
        Paragraph::new(editor_text).wrap(Wrap { trim: false }),
        editor_area,
    );

    // Action bar with a focus-aware navigation hint.
    let in_actions = dialog.focus.is_actions();
    let confirm_selected = dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm;
    let (confirm_style, cancel_style) = if in_actions {
        if confirm_selected {
            (
                engine
                    .bg(palette.status_success)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
            )
        } else {
            (
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
                engine
                    .bg(palette.button_active_bg)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
            )
        }
    } else {
        (
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
        )
    };
    let nav_hint = if dialog.focus.is_actions() {
        active_t("dialog.review_launch.hint_actions")
    } else {
        active_t("dialog.review_launch.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    let actions = Line::from(vec![
        Span::styled(review_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(review_cancel_label(), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    render_dialog_footer(frame, footer_area, &verb_hint(nav_hint));
    // Contextual directory completion of the active path row: a compact menu
    // docked under the path block that lists the repository prefixes continuing
    // the typed text. `Tab` accepts the highlighted row, the arrows walk it, and
    // a click resolves through the same rectangles (c-5, Conventions §8.3).
    if geometry.completions.height > 0 {
        let candidates = dialog.path_completion.candidates();
        let visible_rows = geometry.completions.height.saturating_sub(2) as usize;
        let start = dialog.path_completion.visible_start(visible_rows);
        let menu_block = Block::default()
            .borders(Borders::ALL)
            .border_style(engine.fg(palette.button_active_bg))
            .style(Style::default().bg(engine.color(palette.overlay)));
        let menu_inner = menu_block.inner(geometry.completions);
        frame.render_widget(menu_block, geometry.completions);

        let mut menu_lines = Vec::new();
        for (offset, candidate) in candidates.iter().skip(start).take(visible_rows).enumerate() {
            let index = start + offset;
            let is_selected = index == dialog.path_completion.selected_index();
            let marker = if is_selected { " ▶ " } else { "   " };
            let marker_style = if is_selected {
                engine.fg(palette.status_success).bold()
            } else {
                engine.fg(palette.text_muted)
            };
            let row_style = if is_selected {
                engine
                    .bg(palette.overlay)
                    .fg(engine.color(palette.text_primary))
                    .bold()
            } else {
                engine.fg(palette.text_secondary)
            };
            menu_lines.push(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(candidate.clone(), row_style),
            ]));
        }
        frame.render_widget(Paragraph::new(menu_lines), menu_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a review dialog over the given reviewer count, repository files,
    /// and review presets, so every geometry test exercises the live dialog
    /// state the renderer and the hit-testers share (c-5).
    fn review_dialog_fixture(
        reviewer_count: usize,
        repo_files: &[&str],
        presets: &[tauqe_protocol::ReviewPresetSummary],
    ) -> crate::app::ReviewDialogState {
        let entries = (0..reviewer_count)
            .map(|index| {
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter(format!("reviewer-{index}")),
                    None,
                )
            })
            .collect();
        crate::app::ReviewDialogState::new(
            3,
            10,
            entries,
            repo_files.iter().map(|path| path.to_string()).collect(),
        )
        .with_presets(presets.to_vec())
    }

    #[test]
    fn test_review_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 40);
        let files = ["a/lib.rs", "b/lib.rs", "c/lib.rs", "d/lib.rs"];
        let dialog = review_dialog_fixture(3, &files, &[]);
        let geo = review_dialog_geometry(term, &dialog);
        assert_eq!(geo.preset_rows.height, 0, "no presets, no section");
        assert!(geo.area.width > 0 && geo.area.height > 0);
        assert!(geo.confirm_button.width > 0 && geo.cancel_button.width > 0);
        assert_eq!(geo.confirm_button.y, geo.cancel_button.y);
        assert!(geo.confirm_button.right() <= geo.area.right());
        assert!(geo.cancel_button.right() <= geo.area.right());
        assert!(geo.confirm_button.right() < geo.cancel_button.x);

        assert!(geo.model_rows.y >= geo.area.y);
        assert!(geo.model_rows.y + geo.model_rows.height <= geo.area.bottom());
        assert_eq!(geo.model_rows.height, 3, "one row per reviewer entry");

        assert!(geo.path_area.y >= geo.model_rows.bottom());
        assert!(geo.editor_area.y >= geo.path_area.bottom());
        assert!(geo.editor_area.bottom() <= geo.area.bottom());
    }

    /// A project that declares presets reserves a selectable section between
    /// the reviewer list and the target field, while a project without them
    /// collapses it to zero rows (c-5).
    #[test]
    fn test_review_dialog_geometry_reserves_the_preset_section() {
        let term = Rect::new(0, 0, 120, 50);
        let files = ["a/lib.rs", "b/lib.rs", "c/lib.rs"];
        let presets = [
            tauqe_protocol::ReviewPresetSummary {
                name: "backend".to_string(),
                title: None,
            },
            tauqe_protocol::ReviewPresetSummary {
                name: "frontend".to_string(),
                title: None,
            },
        ];
        let without_presets = review_dialog_fixture(2, &files, &[]);
        let with_presets = review_dialog_fixture(2, &files, &presets);
        let without_geo = review_dialog_geometry(term, &without_presets);
        let with_geo = review_dialog_geometry(term, &with_presets);

        assert_eq!(without_geo.preset_rows.height, 0);
        assert!(with_geo.preset_rows.height > 0);
        assert!(with_geo.preset_rows.y >= with_geo.model_rows.bottom());
        assert!(with_geo.preset_rows.bottom() <= with_geo.path_area.y);
        assert!(with_geo.editor_area.height < without_geo.editor_area.height);
    }

    /// A highlighted preset owns the launch, so its input aid — the target
    /// field — leaves the card entirely, while the preset section itself stays
    /// visible as the mode switcher (c-5).
    #[test]
    fn test_review_dialog_geometry_hides_custom_sections_under_a_preset() {
        let term = Rect::new(0, 0, 120, 50);
        let files = ["a/lib.rs", "b/lib.rs", "c/lib.rs"];
        let presets = [tauqe_protocol::ReviewPresetSummary {
            name: "backend".to_string(),
            title: None,
        }];
        let mut dialog = review_dialog_fixture(2, &files, &presets);
        let custom_geo = review_dialog_geometry(term, &dialog);
        assert!(custom_geo.path_area.height > 0);

        let rows = dialog.preset_selector.item_count();
        dialog.preset_selector.select_index(1, rows);
        assert!(dialog.selected_preset().is_some());
        assert!(!dialog.is_custom_launch());

        let preset_geo = review_dialog_geometry(term, &dialog);
        assert!(
            preset_geo.preset_rows.height > 0,
            "the preset section stays as the mode switcher"
        );
        assert_eq!(preset_geo.path_area.height, 0);
        assert!(preset_geo.editor_area.height > custom_geo.editor_area.height);
    }

    /// The target field keeps the shared picker field height plus exactly one
    /// row for the live match counter, so the input line and its feedback never
    /// collide (c-2, c-4).
    #[test]
    fn test_review_dialog_path_block_reserves_the_counter_row() {
        let term = Rect::new(0, 0, 120, 50);
        let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
        let chunks = super::super::picker_dialog_chunks(
            area,
            3,
            REVIEW_HEADER_HEIGHT,
            review_dialog_path_height(1),
        );

        assert_eq!(
            review_dialog_path_height(1),
            super::super::PICKER_DIALOG_PATH_HEIGHT + 1
        );
        assert_eq!(chunks.path.height, review_dialog_path_height(1));
    }

    /// The target field is fully visible on a normal terminal, and every
    /// neighbouring section is separated by exactly one blank row inside the
    /// card's air-aware body (Conventions §6.7, §6.8).
    #[test]
    fn test_review_dialog_chunks_keep_air_between_sections() {
        let term = Rect::new(0, 0, 120, 50);
        let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
        let chunks = super::super::picker_dialog_chunks(
            area,
            3,
            REVIEW_HEADER_HEIGHT,
            review_dialog_path_height(1),
        );
        let super::super::PickerDialogChunks {
            header,
            list,
            path,
            editor,
            buttons,
            footer,
        } = chunks;

        assert_eq!(path.height, review_dialog_path_height(1));

        // One blank row under the border title and one column of margin on each
        // side, so no section presses against the card frame.
        assert_eq!(header.y, area.y + 2, "border + top inset");
        assert_eq!(header.x, area.x + 2, "border + horizontal padding");
        assert_eq!(header.width, area.width - 4);

        // Header, list, path field, and editor are separate zones separated by
        // exactly one blank row each.
        assert_eq!(list.y, header.y + header.height + 1);
        assert_eq!(path.y, list.y + list.height + 1);
        assert_eq!(editor.y, path.y + path.height + 1);
        assert!(editor.height > 0);

        // The action bar never touches the editor, and the hint strip is
        // docked to the bottom inner row.
        assert_eq!(buttons.y, editor.y + editor.height + 1, "pre-action spacer");
        assert_eq!(footer.y, buttons.y + buttons.height);
        assert_eq!(footer.height, 1, "docked hint strip is one row");
        assert_eq!(footer.y + footer.height, area.bottom() - 1);
    }

    /// The preset section sits between the reviewer list and the target field,
    /// separated from both by exactly one blank row, and keeps its rows on a
    /// normal terminal (Conventions §6.7, c-5).
    #[test]
    fn test_review_dialog_chunks_keep_air_around_the_preset_section() {
        let term = Rect::new(0, 0, 120, 50);
        let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
        let (chunks, presets) = super::super::picker_dialog_chunks_with_presets(
            area,
            3,
            REVIEW_HEADER_HEIGHT,
            review_dialog_path_height(1),
            preset_section_rows(3),
        );
        let super::super::PickerDialogChunks {
            header,
            list,
            path,
            editor,
            ..
        } = chunks;

        assert_eq!(header.y, area.y + 2, "border + top inset");
        assert_eq!(header.y, area.y + 2, "border + top inset");
        assert_eq!(presets.y, list.y + list.height + 1);
        assert_eq!(presets.height, 3 + 2, "rows plus borders");
        assert_eq!(path.y, presets.y + presets.height + 1);
        assert_eq!(editor.y, path.y + path.height + 1);
        assert!(editor.height > 0);
    }

    /// The path block grows with the rows `C-j` adds, up to the cap, so the
    /// entered scope stays on screen without starving the editor (c-3).
    #[test]
    fn test_review_dialog_path_block_grows_with_the_rows() {
        assert_eq!(review_dialog_path_height(1), 4);
        assert_eq!(review_dialog_path_height(2), 5);
        assert_eq!(
            review_dialog_path_height(REVIEW_DIALOG_PATH_MAX_ROWS + 2),
            REVIEW_DIALOG_PATH_MAX_ROWS as u16 + 3,
            "the block is capped so the editor and the action bar survive"
        );

        let term = Rect::new(0, 0, 120, 50);
        let mut dialog = review_dialog_fixture(2, &["a/lib.rs", "b/lib.rs"], &[]);
        dialog.path_inputs[0].insert_str("a/");
        assert!(dialog.append_path_row());

        let one_row = review_dialog_geometry(term, &review_dialog_fixture(2, &["a/lib.rs"], &[]));
        let two_rows = review_dialog_geometry(term, &dialog);
        assert_eq!(two_rows.path_rows.height, 2);
        assert!(two_rows.path_area.height > one_row.path_area.height);
        assert!(two_rows.editor_area.height < one_row.editor_area.height);
        assert!(two_rows.path_rows.bottom() <= two_rows.path_area.bottom());
    }

    /// The completion menu drops from the path block, stays above the action bar,
    /// and disappears when the active row offers no candidate (c-5).
    #[test]
    fn test_review_dialog_completion_menu_sits_between_the_paths_and_the_actions() {
        let term = Rect::new(0, 0, 120, 50);
        let mut dialog = review_dialog_fixture(2, &["crates/core/src/lib.rs"], &[]);
        let closed = review_dialog_geometry(term, &dialog);
        assert_eq!(closed.completions.height, 0, "no candidate, no menu");

        // The menu belongs to the row being typed: it exists only while the
        // target-paths section owns the keyboard, so the focus moves there
        // before the geometry is evaluated (c-5).
        dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
        dialog.path_inputs[0].insert_str("cra");
        dialog.refresh_path_completion();
        let open = review_dialog_geometry(term, &dialog);
        assert!(open.completions.height > 0);
        assert_eq!(open.completions.x, open.path_area.x);
        assert_eq!(open.completions.y, open.path_area.bottom());
        assert!(
            open.completions.bottom() <= open.confirm_button.y,
            "the menu never covers the action bar"
        );
    }

    /// Draws one review-dialog frame and returns its rows as text, so a test can
    /// assert what the developer actually reads on the card.
    fn rendered_rows(dialog: &crate::app::ReviewDialogState) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(120, 50)).expect("test backend");
        terminal
            .draw(|frame| render_review_dialog(frame, dialog, PrimaryModifier::Ctrl))
            .expect("the review dialog frame draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// The active target row carries a full-width band while the other rows stay
    /// plain, so the row being typed reads as the selected row of the list and
    /// its empty cells belong to the same band (c-3, Conventions §8.3).
    #[test]
    fn test_path_row_line_bands_only_the_active_row() {
        let engine = current_engine();
        let spans = vec![Span::raw("  > crates/")];

        let active = path_row_line(spans.clone(), true, 24, &engine);
        let plain = path_row_line(spans, false, 24, &engine);

        assert_eq!(active.width(), 24, "the band fills the whole inner width");
        assert_ne!(active.style, Style::default(), "the active row is filled");
        assert_eq!(plain.width(), "  > crates/".len());
        assert_eq!(plain.style, Style::default(), "other rows stay plain");

        // A row wider than the column budget keeps its content instead of being
        // truncated by the band.
        let overflow = path_row_line(vec![Span::raw("x".repeat(40))], true, 24, &engine);
        assert_eq!(overflow.width(), 40);
    }

    /// The entered patterns are drawn top-down in entry order and the union
    /// counter sits under the list, naming the whole entered scope (c-3, c-6).
    #[test]
    fn test_review_dialog_renders_the_path_rows_and_one_union_counter() {
        let mut dialog = review_dialog_fixture(1, &["crates/core/src/lib.rs", "README.md"], &[]);
        dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
        dialog.path_inputs[0].insert_str("crates/");
        assert!(dialog.append_path_row());
        dialog.path_inputs[1].insert_str("README.md");
        dialog.close_path_completion();

        let rows = rendered_rows(&dialog);

        let first = rows
            .iter()
            .position(|row| row.contains("> crates/"))
            .expect("the first row is drawn");
        let second = rows
            .iter()
            .position(|row| row.contains("> README.md"))
            .expect("the second row is drawn");
        assert!(first < second, "the rows keep their entry order");

        let counter = active_t("dialog.review_launch.counter_matches").replacen("{}", "2", 1);
        let counter_row = rows
            .iter()
            .position(|row| row.contains(&counter))
            .expect("one union counter under the list");
        assert!(counter_row > second, "the counter sits under the list");
    }

    /// Typing opens the contextual directory menu as a compact list that marks
    /// the highlighted candidate and grows downward toward the action bar (c-5).
    #[test]
    fn test_review_dialog_renders_the_directory_completion_menu() {
        let mut dialog = review_dialog_fixture(1, &["crates/core/src/lib.rs"], &[]);
        dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
        dialog.path_inputs[0].insert_str("cra");
        dialog.refresh_path_completion();

        let rows = rendered_rows(&dialog);

        let highlighted = rows
            .iter()
            .position(|row| row.contains("▶ crates/"))
            .expect("the highlighted candidate is drawn");
        let deeper = rows
            .iter()
            .position(|row| row.contains("crates/core/"))
            .expect("the menu lists the deeper prefix");
        assert!(deeper > highlighted, "the menu grows downward");
    }
}
