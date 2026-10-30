//! Single-target review execution dialog: Coder picker and directives editor.
//!
//! Point execution of a finding or a subplan step exposes the Coder ladder and a
//! directives editor, so the developer picks the implementing model and adds
//! instructions before a bounded turn spends tokens. The layout mirrors the
//! Architect refinement dialogs — header, embedded model list, editor, action
//! bar, docked hint strip — so both pickers read identically (concept #c-3,
//! Conventions §6, §7).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::AppState;
use crate::components::modal::verb_hint;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::header::truncate_to_width;

/// Width of the single-target execution dialog, as a percentage of the terminal.
const EXECUTE_REVIEW_DIALOG_WIDTH: u16 = 68;
/// Height of the single-target execution dialog, as a percentage of the terminal.
const EXECUTE_REVIEW_DIALOG_HEIGHT: u16 = 56;
/// Header rows: the review, the finding, an optional step, and target files.
const EXECUTE_REVIEW_HEADER_HEIGHT: u16 = 4;

/// Full drawn confirm-button label, resolved from the runtime locale (c-1).
fn execute_review_confirm_label() -> String {
    format!(" [ {} ] ", active_t("dialog.button.resolve"))
}

/// Full drawn cancel-button label, resolved from the runtime locale (c-1).
fn execute_review_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Rectangles of the single-target execution dialog, shared by the renderer and
/// the mouse hit-tester so the surface that is drawn is the surface that is
/// clicked (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteReviewDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the Coder model list (one row per entry).
    pub model_rows: Rect,
    /// The directives editor block.
    pub editor_area: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Computes the dialog frame, the Coder-row area, and the action buttons from
/// the shared picker layout the renderer uses, so the drawn buttons are always
/// the clickable ones (Conventions §6, §7).
pub fn execute_review_dialog_geometry(
    term: Rect,
    entry_count: usize,
) -> ExecuteReviewDialogGeometry {
    let area = crate::ui::centered_rect(
        EXECUTE_REVIEW_DIALOG_WIDTH,
        EXECUTE_REVIEW_DIALOG_HEIGHT,
        term,
    );
    let chunks = super::picker_dialog_chunks(area, entry_count, EXECUTE_REVIEW_HEADER_HEIGHT, 0);
    let model_rows = super::inset(chunks.list);

    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) = super::picker_button_rects(
        chunks.buttons,
        &execute_review_confirm_label(),
        &execute_review_cancel_label(),
    );

    ExecuteReviewDialogGeometry {
        area,
        model_rows,
        editor_area: chunks.editor,
        confirm_button,
        cancel_button,
    }
}

/// Renders the single-target review-execution confirmation.
///
/// Point execution exposes the Coder ladder and a directives editor, so the
/// developer picks the implementing model and adds instructions before a bounded
/// turn spends tokens. The layout mirrors the Architect refinement dialogs —
/// header, embedded model list, editor, action bar, docked hint strip — so both
/// pickers read identically (concept #c-3, Conventions §6, §7).
pub fn render_execute_review_dialog(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = state.confirm_execute_review.as_ref() else {
        return;
    };

    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let entry_count = target.selector.len();
    let area = crate::ui::centered_rect(
        EXECUTE_REVIEW_DIALOG_WIDTH,
        EXECUTE_REVIEW_DIALOG_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(
            " {} ",
            active_t("dialog.execute_review.title_point")
        ))
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
    } = super::picker_dialog_chunks(area, entry_count, EXECUTE_REVIEW_HEADER_HEIGHT, 0);

    // Header: which review, finding, and step are about to be resolved.
    let mut review_spans = vec![
        Span::styled(active_t("dialog.execute_review.review_label"), dim),
        Span::styled(
            target.review_title.clone(),
            engine.fg(palette.text_primary).bold(),
        ),
    ];
    if let Some(review_id) = target.review_id.as_deref() {
        review_spans.push(Span::styled(format!(" [{}]", review_id), dim));
    }
    let mut header_lines = vec![Line::from(review_spans)];

    // The architectural group owns the unified plan, so the form names it before
    // the step: a group step is addressed through the group that carries the
    // root cause, never through one of its findings (c-1, c-4, c-5).
    if let Some(group_id) = target.group_id.as_deref() {
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_review.group_label"), dim),
            Span::styled(
                truncate_to_width(&target.group_title, 48),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(format!(" [{}]", group_id), dim),
        ]));
    }

    if let Some(item) = target.items.first() {
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_review.finding_label"), dim),
            Span::styled(
                format!("#{} {}", item.id, truncate_to_width(&item.title, 44)),
                engine.fg(palette.text_primary).bold(),
            ),
        ]));
        if !item.files.is_empty() {
            header_lines.push(Line::from(vec![
                Span::styled(active_t("dialog.execute_review.files_label"), dim),
                Span::styled(
                    truncate_to_width(&item.files.join(", "), 58),
                    engine.fg(palette.status_info),
                ),
            ]));
        }
    }

    if let Some(step_id) = target.step_id.as_deref() {
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_review.step_label"), dim),
            Span::styled(
                step_id.to_string(),
                engine.fg(palette.status_warning).bold(),
            ),
        ]));
    }
    // The header band is fixed at EXECUTE_REVIEW_HEADER_HEIGHT rows, so a card
    // that would carry more lines keeps the leading ones instead of painting
    // into the model list below it.
    header_lines.truncate(EXECUTE_REVIEW_HEADER_HEIGHT as usize);
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Coder list. The shared `ModelSelector` owns the rows, numbering, scrolling,
    // and empty-list safety; the block adds framing and the focus color.
    let list_title = if entry_count > 1 {
        active_t("dialog.execute_review.model_list_counter")
            .replacen("{}", &(target.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.execute_review.model_list_title")
    };
    let list_border = if target.focus.is_model_list() {
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
                active_t("dialog.execute_review.no_models"),
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = target
            .selector
            .render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Directives editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = target.focus.is_instructions_editor();
    let mut editor_content = Vec::new();
    if target.instructions_editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.execute_review.editor_placeholder"),
            dim,
        ));
        editor_content.push(Line::from(spans));
    } else {
        for line in super::editor_lines(&target.instructions_editor, show_cursor) {
            let mut spans = vec![Span::raw("> ")];
            spans.extend(line.spans);
            editor_content.push(Line::from(spans));
        }
    }
    let editor_border = if target.focus.is_instructions_editor() {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(
            " {} ",
            active_t("dialog.execute_review.editor_title")
        ))
        .borders(Borders::ALL)
        .border_style(editor_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
        editor_area,
    );

    // Action bar with focus-aware styling.
    let in_actions = target.focus.is_actions();
    let confirm_selected = target.confirm_button == crate::app::ConfirmDialogButton::Confirm;
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
    let actions = Line::from(vec![
        Span::styled(execute_review_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(execute_review_cancel_label(), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The hint is docked to the card's bottom edge as its own strip filled with
    // `palette.overlay`, so it reads as persistent chrome instead of a second
    // row of buttons (Conventions §6.4).
    let primary = state.tui_config.input.primary_modifier;
    let nav_hint = if in_actions {
        active_t("dialog.execute_review.hint_actions")
    } else {
        active_t("dialog.execute_review.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    crate::components::modal::render_dialog_footer(frame, footer_area, &verb_hint(nav_hint));
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::app::ConfirmExecuteReviewState;

    /// A step of an architectural group names the group and the step in its
    /// header band, so a point execution never loses the root cause it belongs
    /// to (c-1, c-4, c-5).
    #[test]
    fn group_step_form_renders_the_group_and_the_step() {
        let mut st = crate::app::test_state();
        st.confirm_execute_review = Some(
            ConfirmExecuteReviewState::new(
                Some("rev-1".to_string()),
                "Session rev-1".to_string(),
                "Unified auth guard".to_string(),
                None,
                Some("1.1".to_string()),
                Vec::new(),
                Vec::new(),
            )
            .with_group("arch-auth".to_string(), "Unified auth guard".to_string()),
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_execute_review_dialog(frame, &st))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for row in 0..40u16 {
            for column in 0..120u16 {
                screen.push_str(buffer[(column, row)].symbol());
            }
            screen.push('\n');
        }
        assert!(screen.contains("Unified auth guard"), "{screen}");
        assert!(screen.contains("[arch-auth]"), "{screen}");
        assert!(screen.contains("1.1"), "{screen}");
    }
}
