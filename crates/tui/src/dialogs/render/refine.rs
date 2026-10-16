//! Rendering for the AI architect plan refinement dialog and its blocked-state
//! popup.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::PrimaryModifier;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;
use super::{editor_lines, render_modal_confirm, ConfirmModalParams, ModalTone};

/// Width of the refine dialog, as a percentage of the terminal width.
pub const REFINE_DIALOG_WIDTH: u16 = 68;
/// Height of the refine dialog, as a percentage of the terminal height.
pub const REFINE_DIALOG_HEIGHT: u16 = 56;
/// Action-bar label for starting a refinement.
pub const REFINE_DIALOG_CONFIRM_LABEL: &str = "[ Start Refinement (C-Enter) ]";
/// Action-bar label for cancelling the refine dialog.
pub const REFINE_DIALOG_CANCEL_LABEL: &str = "[ Cancel ]";

/// Height of the header block (plan label and description).
const REFINE_HEADER_HEIGHT: u16 = 4;
/// Height of the action-bar block (buttons and navigation hint).
const REFINE_FOOTER_HEIGHT: u16 = 3;
/// Minimum height kept for the instructions editor on short terminals.
const REFINE_EDITOR_MIN_HEIGHT: u16 = 3;
/// Maximum architect rows drawn before the list scrolls.
const REFINE_MODEL_LIST_MAX_ROWS: u16 = 6;

/// Number of architect rows drawn for `entry_count` entries, before borders.
fn refine_model_list_rows(entry_count: usize) -> u16 {
    (entry_count.max(1) as u16).min(REFINE_MODEL_LIST_MAX_ROWS)
}

/// Rectangles of the refine dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the architect model list (one row per entry).
    pub model_rows: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Insets a rectangle by a one-cell border on every side.
fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Splits the dialog interior into `[header, architect list, editor, footer]`.
///
/// The action bar, list, and header are sized first so the dialog stays usable
/// on short terminals; the instructions editor absorbs whatever height remains.
fn refine_dialog_chunks(area: Rect, entry_count: usize) -> [Rect; 4] {
    let inner = inset(area);

    let footer_h = REFINE_FOOTER_HEIGHT.min(inner.height);
    let after_footer = inner.height - footer_h;

    // The list never takes more than half of the room left after the footer, so
    // the header and editor always keep space on short terminals.
    let list_h = (refine_model_list_rows(entry_count) + 2).min(after_footer / 2);
    let after_list = after_footer - list_h;

    let header_h = REFINE_HEADER_HEIGHT.min(after_list.saturating_sub(REFINE_EDITOR_MIN_HEIGHT));
    let editor_h = after_list - header_h;

    let header = Rect::new(inner.x, inner.y, inner.width, header_h);
    let list = Rect::new(inner.x, inner.y + header_h, inner.width, list_h);
    let editor = Rect::new(inner.x, list.y + list_h, inner.width, editor_h);
    let footer = Rect::new(inner.x, editor.y + editor_h, inner.width, footer_h);
    [header, list, editor, footer]
}

/// Computes the dialog frame, the architect-row area, and the action buttons
/// from the same layout the renderer uses.
pub fn refine_dialog_geometry(term: Rect, entry_count: usize) -> RefineDialogGeometry {
    let area = centered_rect(REFINE_DIALOG_WIDTH, REFINE_DIALOG_HEIGHT, term);
    let chunks = refine_dialog_chunks(area, entry_count);
    let model_rows = inset(chunks[1]);

    let footer = chunks[3];
    let confirm_w = REFINE_DIALOG_CONFIRM_LABEL.len() as u16 + 2;
    let cancel_w = REFINE_DIALOG_CANCEL_LABEL.len() as u16 + 2;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = footer.x + (footer.width.saturating_sub(total)) / 2;
    let confirm_button = Rect::new(start_x, footer.y, confirm_w, 1);
    let cancel_button = Rect::new(start_x + confirm_w + gap, footer.y, cancel_w, 1);

    RefineDialogGeometry {
        area,
        model_rows,
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
        .title(" Deep Plan Refinement (AI Architect) ")
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.status_info))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let [header_area, list_area, editor_area, footer_area] =
        refine_dialog_chunks(area, entry_count);

    let dim = engine.fg(palette.text_muted);

    // Header: what is about to be refined.
    let header_lines = vec![
        Line::from(vec![
            Span::styled(" Plan: ", dim),
            Span::styled(&dialog.plan_title, engine.fg(palette.text_primary).bold()),
            Span::styled(format!(" [{}]", dialog.plan_id), dim),
        ]),
        Line::from(Span::styled(
            " Decomposes high-level tasks into verified atomic engineering steps without altering agreed architecture.",
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Architect list. The shared `ModelSelector` owns the rows, numbering,
    // scrolling, and empty-list safety; the block adds the framing and the
    // focus color that shows which section owns the keyboard
    // (c-embedded-model-list, c-unified-dialog-integration).
    let list_title = if entry_count > 1 {
        format!(
            " Architect Model  [{} of {}] ",
            dialog.selector.selected_index() + 1,
            entry_count
        )
    } else {
        " Architect Model ".to_string()
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
                "  (no architect models configured)",
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = dialog.selector.render_list_lines(list_inner.height as usize);
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
            " Additional refinement directives (e.g. 'split step 2 into atomic subtasks') [optional]...",
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
        .title(" Architect Instructions (optional) ")
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
                engine.bg(palette.status_success).fg(engine.color(palette.text_inverse)).bold(),
                engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
            )
        } else {
            (
                engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
                engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold(),
            )
        }
    } else {
        (
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg)),
        )
    };
    let nav_hint = if dialog.focus.is_actions() {
        "Enter: confirm • ←/→: button • Esc: cancel".to_string()
    } else {
        format!(
            "{}Enter: start • Tab: focus • {}: model • Esc: cancel",
            primary.prefix_str(),
            crate::components::model_selector::model_select_hint(primary)
        )
    };
    let actions = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", REFINE_DIALOG_CONFIRM_LABEL), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" {} ", REFINE_DIALOG_CANCEL_LABEL), cancel_style),
        ]),
        Line::from(Span::styled(nav_hint, engine.fg(palette.text_secondary))),
    ];
    let footer = Paragraph::new(actions).alignment(Alignment::Center);
    frame.render_widget(footer, footer_area);
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
            "AI Architect Refused Decomposition (Blockers Detected)",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Plan: ", engine.fg(palette.text_muted)),
            Span::styled(
                format!("{} [{}]", blocked.plan_title, blocked.plan_id),
                engine.fg(palette.text_primary).bold(),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled("Reason / Contradiction:", engine.fg(palette.status_error).bold())),
    ];

    for line in blocked.reason.lines() {
        body_lines.push(Line::from(Span::styled(format!("  {}", line), engine.fg(palette.text_primary))));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE prohibits speculative guessing. Replan uncompleted horizon addressing architectural blockers.",
        engine.fg(palette.text_muted),
    )));

    ConfirmModalParams {
        title: "Plan Refinement Blocked",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Replan Horizon (Y)",
        cancel_label: "Dismiss (Esc)",
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
    }
}
