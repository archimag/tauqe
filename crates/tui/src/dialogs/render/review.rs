use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::editor_lines;
use crate::components::modal::{render_dialog_footer, verb_hint};
use crate::config::PrimaryModifier;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;

/// Width of the review launch dialog, as a percentage of the terminal width.
pub const REVIEW_DIALOG_WIDTH: u16 = 64;
/// Height of the review launch dialog, as a percentage of the terminal height.
///
/// The card stacks five heterogeneous zones — header, reviewer list, target
/// field, instructions editor — with one blank row between each of them, plus
/// the action bar and its docked hint strip. It is therefore taller than a plain
/// form dialog so the air never costs a section (Conventions §6.7).
pub const REVIEW_DIALOG_HEIGHT: u16 = 70;
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
    /// The target-paths input block.
    pub path_area: Rect,
    /// The instruction editor block.
    pub editor_area: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Computes the dialog frame, the reviewer-row area, and the action buttons
/// from the same layout the renderer uses.
pub fn review_dialog_geometry(term: Rect, entry_count: usize) -> ReviewDialogGeometry {
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
    let chunks = super::picker_dialog_chunks(
        area,
        entry_count,
        REVIEW_HEADER_HEIGHT,
        super::PICKER_DIALOG_PATH_HEIGHT,
    );
    let model_rows = super::inset(chunks.list);

    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) = super::picker_button_rects(
        chunks.buttons,
        &review_confirm_label(),
        &review_cancel_label(),
    );

    ReviewDialogGeometry {
        area,
        model_rows,
        path_area: chunks.path,
        editor_area: chunks.editor,
        confirm_button,
        cancel_button,
    }
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

    let super::PickerDialogChunks {
        header: header_area,
        list: list_area,
        path: path_area,
        editor: editor_area,
        buttons: buttons_area,
        footer: footer_area,
    } = super::picker_dialog_chunks(
        area,
        entry_count,
        REVIEW_HEADER_HEIGHT,
        super::PICKER_DIALOG_PATH_HEIGHT,
    );

    let dim = engine.fg(palette.text_muted);

    // Header: what is about to be reviewed.
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
        Line::from(Span::styled(active_t("dialog.review_launch.note"), dim)),
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

    // Target paths field. A single-line input that narrows the review to a
    // directory or glob without touching the User Context layer: the value rides
    // along in `ReviewStartParams::path_pattern` and the server merges the
    // matched files with the developer's layer (c-review-context-input). The
    // cursor is drawn only when the field owns focus.
    let show_path_cursor = dialog.focus.is_path_editor();
    let path_border = if show_path_cursor {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let mut path_content = Vec::new();
    if dialog.path_editor.is_empty() {
        let mut spans = vec![Span::raw("  > ")];
        if show_path_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.review_launch.path_placeholder"),
            dim,
        ));
        path_content.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.path_editor, show_path_cursor) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(line.spans);
            path_content.push(Line::from(spans));
        }
    }
    let path_block = Block::default()
        .title(format!(" {} ", active_t("dialog.review_launch.path_title")))
        .borders(Borders::ALL)
        .border_style(path_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(path_content)
            .block(path_block)
            .wrap(Wrap { trim: false }),
        path_area,
    );

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 40);
        let geo = review_dialog_geometry(term, 3);
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

    /// The reserved single-line path field is fully visible on a normal terminal,
    /// and every neighbouring section is separated by exactly one blank row
    /// inside the card's air-aware body (Conventions §6.7, §6.8).
    #[test]
    fn test_review_dialog_chunks_keep_air_between_sections() {
        let term = Rect::new(0, 0, 120, 50);
        let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
        let super::super::PickerDialogChunks {
            header,
            list,
            path,
            editor,
            buttons,
            footer,
        } = super::super::picker_dialog_chunks(
            area,
            3,
            REVIEW_HEADER_HEIGHT,
            super::super::PICKER_DIALOG_PATH_HEIGHT,
        );

        assert_eq!(path.height, super::super::PICKER_DIALOG_PATH_HEIGHT);

        // One blank row under the border title and one column of margin on each
        // side, so no section presses against the card frame.
        assert_eq!(header.y, area.y + 2, "border + top inset");
        assert_eq!(header.x, area.x + 2, "border + horizontal padding");
        assert_eq!(header.width, area.width - 4);

        // Header, list, path field, and editor are four separate zones.
        assert_eq!(list.y, header.y + header.height + 1);
        assert_eq!(path.y, list.y + list.height + 1);
        assert_eq!(editor.y, path.y + path.height + 1);
        assert!(editor.height > 0);

        // The action bar never touches the editor, and the hint strip is docked
        // to the bottom inner row.
        assert_eq!(buttons.y, editor.y + editor.height + 1, "pre-action spacer");
        assert_eq!(footer.y, buttons.y + buttons.height);
        assert_eq!(footer.height, 1, "docked hint strip is one row");
        assert_eq!(footer.y + footer.height, area.bottom() - 1);
    }
}
