//! Rendering for the AI architect plan refinement dialog and its blocked-state
//! popup.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{editor_lines, render_modal_confirm, ConfirmModalParams, ModalTone};
use crate::components::modal::render_dialog_footer;
use crate::config::PrimaryModifier;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;

/// Width of the refine dialog, as a percentage of the terminal width.
pub const REFINE_DIALOG_WIDTH: u16 = 68;
/// Height of the refine dialog, as a percentage of the terminal height.
pub const REFINE_DIALOG_HEIGHT: u16 = 56;
/// Full drawn confirm-button label, resolved from the runtime locale.
///
/// Rendering and mouse hit-testing both measure this label, so the clickable
/// button can never drift from the drawn one (Conventions §6, §9, c-1).
fn refine_confirm_label() -> String {
    format!(" [ {} ] ", active_t("modal.start_refinement"))
}

/// Full drawn cancel-button label, resolved from the runtime locale.
fn refine_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Height of the header block (plan label and description).
const REFINE_HEADER_HEIGHT: u16 = 4;

/// Rectangles of the refine dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the architect model list (one row per entry).
    pub model_rows: Rect,
    /// The instructions editor block.
    pub editor_area: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Computes the dialog frame, the architect-row area, and the action buttons
/// from the same layout the renderer uses.
pub fn refine_dialog_geometry(term: Rect, entry_count: usize) -> RefineDialogGeometry {
    let area = centered_rect(REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT, term);
    let chunks = super::picker_dialog_chunks(area, entry_count, REFINE_HEADER_HEIGHT, 0);
    let model_rows = super::inset(chunks.list);

    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) = super::picker_button_rects(
        chunks.buttons,
        &refine_confirm_label(),
        &refine_cancel_label(),
    );

    RefineDialogGeometry {
        area,
        model_rows,
        editor_area: chunks.editor,
        confirm_button,
        cancel_button,
    }
}

pub fn render_plan_refine_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::views::plans::PlanRefineDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let entry_count = dialog.selector.len();
    let area = centered_rect(REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", active_t("dialog.refine.title")))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.status_info))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let super::PickerDialogChunks {
        header: header_area,
        list: list_area,
        editor: editor_area,
        buttons: buttons_area,
        footer: footer_area,
        ..
    } = super::picker_dialog_chunks(area, entry_count, REFINE_HEADER_HEIGHT, 0);

    let dim = engine.fg(palette.text_muted);

    // Header: what is about to be refined.
    let header_lines = vec![
        Line::from(vec![
            Span::styled(active_t("dialog.refine.plan_label"), dim),
            Span::styled(&dialog.plan_title, engine.fg(palette.text_primary).bold()),
            Span::styled(format!(" [{}]", dialog.plan_id), dim),
        ]),
        Line::from(Span::styled(active_t("dialog.refine.header_note"), dim)),
    ];
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Architect list. The shared `ModelSelector` owns the rows, numbering,
    // scrolling, and empty-list safety; the block adds the framing and the
    // focus color that shows which section owns the keyboard
    // (c-embedded-model-list, c-unified-dialog-integration).
    let list_title = if entry_count > 1 {
        active_t("dialog.refine.model_list_counter")
            .replacen("{}", &(dialog.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.refine.model_list_title")
    };
    let list_border = if dialog.focus.is_model_list() {
        engine.fg(palette.status_info).bold()
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
                active_t("dialog.refine.no_models"),
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

    // Instructions editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = dialog.focus.is_instructions_editor();
    let mut editor_content = Vec::new();
    if dialog.instructions_editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.refine.editor_placeholder"),
            dim,
        ));
        editor_content.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.instructions_editor, show_cursor) {
            let mut spans = vec![Span::raw("> ")];
            spans.extend(line.spans);
            editor_content.push(Line::from(spans));
        }
    }
    let editor_border = if dialog.focus.is_instructions_editor() {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(" {} ", active_t("dialog.refine.editor_title")))
        .borders(Borders::ALL)
        .border_style(editor_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
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
        active_t("dialog.refine.hint_actions")
    } else {
        active_t("dialog.refine.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    let actions = Line::from(vec![
        Span::styled(refine_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(refine_cancel_label(), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome instead
    // of a second line of buttons (c-1, c-2).
    render_dialog_footer(frame, footer_area, &nav_hint);
}

/// Renders the review-finding decomposition dialog.
///
/// Mirrors the plan refinement dialog — the Architect model list, an optional
/// instructions editor, and the standard action bar with a docked hint strip —
/// but names the review session and the finding it decomposes. Reusing the same
/// chunks, labels, and [`refine_dialog_geometry`] keeps the drawn buttons and the
/// mouse hit targets identical across both refinement dialogs (Conventions §7).
pub fn render_review_refine_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::views::review::ReviewRefineDialogState,
    primary: PrimaryModifier,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let entry_count = dialog.selector.len();
    let area = centered_rect(REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", active_t("dialog.review_refine.title")))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.button_active_bg))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let super::PickerDialogChunks {
        header: header_area,
        list: list_area,
        editor: editor_area,
        buttons: buttons_area,
        footer: footer_area,
        ..
    } = super::picker_dialog_chunks(area, entry_count, REFINE_HEADER_HEIGHT, 0);

    let dim = engine.fg(palette.text_muted);

    // Header: which finding is about to be decomposed.
    let header_lines = vec![
        Line::from(vec![
            Span::styled(active_t("dialog.review_refine.review_label"), dim),
            Span::styled(
                dialog.review_title.clone(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(format!(" [{}]", dialog.review_id), dim),
        ]),
        Line::from(vec![
            Span::styled(active_t("dialog.review_refine.finding_label"), dim),
            Span::styled(
                format!("#{} {}", dialog.item_id, dialog.item_title),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.review_refine.header_note"),
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Architect list: the shared `ModelSelector` owns rows, numbering,
    // scrolling, and empty-list safety; the block adds framing and the focus
    // color that shows which section owns the keyboard.
    let list_title = if entry_count > 1 {
        active_t("dialog.refine.model_list_counter")
            .replacen("{}", &(dialog.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.refine.model_list_title")
    };
    let list_border = if dialog.focus.is_model_list() {
        engine.fg(palette.status_info).bold()
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
                active_t("dialog.refine.no_models"),
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

    // Instructions editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = dialog.focus.is_instructions_editor();
    let mut editor_content = Vec::new();
    if dialog.instructions_editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.refine.editor_placeholder"),
            dim,
        ));
        editor_content.push(Line::from(spans));
    } else {
        for line in editor_lines(&dialog.instructions_editor, show_cursor) {
            let mut spans = vec![Span::raw("> ")];
            spans.extend(line.spans);
            editor_content.push(Line::from(spans));
        }
    }
    let editor_border = if dialog.focus.is_instructions_editor() {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(" {} ", active_t("dialog.refine.editor_title")))
        .borders(Borders::ALL)
        .border_style(editor_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
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
        active_t("dialog.refine.hint_actions")
    } else {
        active_t("dialog.refine.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    let actions = Line::from(vec![
        Span::styled(refine_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(refine_cancel_label(), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The navigation hint is docked to the card's bottom edge as its own strip
    // filled with `palette.overlay`, so it reads as persistent chrome.
    render_dialog_footer(frame, footer_area, &nav_hint);
}

/// Presentation parameters for the "refinement blocked" modal, shared by the
/// renderer and mouse hit-testing so the drawn action buttons are always the
/// clickable ones (Conventions §6, §7).
pub fn refine_blocked_params<'a>(
    blocked: &'a crate::views::plans::PlanRefineBlockedState,
    confirm_button: crate::app::ConfirmDialogButton,
) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.refine_blocked.heading"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.refine_blocked.plan_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                format!("{} [{}]", blocked.plan_title, blocked.plan_id),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.refine_blocked.reason_label"),
            engine.fg(palette.status_error).bold(),
        )),
    ];

    for line in blocked.reason.lines() {
        body_lines.push(Line::from(Span::styled(
            format!("  {}", line),
            engine.fg(palette.text_primary),
        )));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.refine_blocked.note"),
        engine.fg(palette.text_muted),
    )));

    ConfirmModalParams {
        title: active_t("dialog.refine_blocked.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.replan_horizon"),
        cancel_label: active_t("dialog.button.dismiss"),
        confirm_button,
        destructive: false,
        width: 70,
    }
}

pub fn render_plan_refine_blocked_dialog(
    frame: &mut ratatui::Frame,
    blocked: &crate::views::plans::PlanRefineBlockedState,
    confirm_button: crate::app::ConfirmDialogButton,
) {
    render_modal_confirm(frame, refine_blocked_params(blocked, confirm_button));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_refine_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 40);
        let geo = refine_dialog_geometry(term, 3);
        assert!(geo.area.width > 0 && geo.area.height > 0);
        assert!(geo.confirm_button.width > 0 && geo.cancel_button.width > 0);
        assert_eq!(geo.confirm_button.y, geo.cancel_button.y);
        assert!(geo.confirm_button.right() <= geo.area.right());
        assert!(geo.cancel_button.right() <= geo.area.right());
        assert!(geo.confirm_button.right() < geo.cancel_button.x);

        assert!(geo.model_rows.y >= geo.area.y);
        assert!(geo.model_rows.y + geo.model_rows.height <= geo.area.bottom());
        assert_eq!(geo.model_rows.height, 3, "one row per architect entry");

        assert!(geo.editor_area.y >= geo.model_rows.bottom());
        assert!(geo.editor_area.bottom() <= geo.area.bottom());
    }

    /// The architect list and the instructions editor are distinct zones: a
    /// blank row sits under the header, between the list and the editor, and
    /// above the action bar (Conventions §6.7, §6.8).
    #[test]
    fn refine_dialog_sections_are_separated_by_blank_rows() {
        let term = Rect::new(0, 0, 120, 50);
        let area = centered_rect(REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT, term);
        let chunks = super::super::picker_dialog_chunks(area, 3, REFINE_HEADER_HEIGHT, 0);

        assert_eq!(chunks.header.y, area.y + 2, "border + top inset");
        assert_eq!(chunks.header.x, area.x + 2, "border + horizontal padding");
        assert_eq!(chunks.header.width, area.width - 4);

        assert_eq!(chunks.list.y, chunks.header.y + chunks.header.height + 1);
        assert_eq!(chunks.path.height, 0, "this dialog has no target field");
        assert_eq!(chunks.editor.y, chunks.list.y + chunks.list.height + 1);
        assert!(chunks.editor.height > 0);

        assert_eq!(
            chunks.buttons.y,
            chunks.editor.y + chunks.editor.height + 1,
            "pre-action spacer"
        );
        assert_eq!(
            chunks.footer.y + chunks.footer.height,
            area.bottom() - 1,
            "hint strip is docked above the bottom border"
        );
    }
}
