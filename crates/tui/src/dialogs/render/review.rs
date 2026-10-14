use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::PrimaryModifier;
use crate::ui::centered_rect;
use super::editor_lines;

/// Width of the review launch dialog, as a percentage of the terminal width.
pub const REVIEW_DIALOG_WIDTH: u16 = 64;
/// Height of the review launch dialog, as a percentage of the terminal height.
pub const REVIEW_DIALOG_HEIGHT: u16 = 54;
/// Action-bar label for starting a review.
pub const REVIEW_DIALOG_CONFIRM_LABEL: &str = "[ Start Review ]";
/// Action-bar label for cancelling the review dialog.
pub const REVIEW_DIALOG_CANCEL_LABEL: &str = "[ Cancel ]";

/// Height of the header block (summary and target metadata).
const REVIEW_HEADER_HEIGHT: u16 = 6;
/// Height of the action-bar block (buttons and navigation hint).
const REVIEW_FOOTER_HEIGHT: u16 = 3;
/// Minimum height kept for the instruction editor on short terminals.
const REVIEW_EDITOR_MIN_HEIGHT: u16 = 3;
/// Maximum reviewer rows drawn before the list scrolls.
const REVIEW_MODEL_LIST_MAX_ROWS: u16 = 6;

/// Number of reviewer rows drawn for `entry_count` entries, before borders.
fn review_model_list_rows(entry_count: usize) -> u16 {
    (entry_count.max(1) as u16).min(REVIEW_MODEL_LIST_MAX_ROWS)
}

/// Rectangles of the review dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the reviewer model list (one row per entry).
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

/// Splits the dialog interior into `[header, reviewer list, editor, footer]`.
///
/// The action bar, list, and header are sized first so the dialog stays usable
/// on short terminals; the instruction editor absorbs whatever height remains.
fn review_dialog_chunks(area: Rect, entry_count: usize) -> [Rect; 4] {
    let inner = inset(area);

    let footer_h = REVIEW_FOOTER_HEIGHT.min(inner.height);
    let after_footer = inner.height - footer_h;

    // The list never takes more than half of the room left after the footer, so
    // the header and editor always keep space on short terminals.
    let list_h = (review_model_list_rows(entry_count) + 2).min(after_footer / 2);
    let after_list = after_footer - list_h;

    let header_h = REVIEW_HEADER_HEIGHT.min(after_list.saturating_sub(REVIEW_EDITOR_MIN_HEIGHT));
    let editor_h = after_list - header_h;

    let header = Rect::new(inner.x, inner.y, inner.width, header_h);
    let list = Rect::new(inner.x, inner.y + header_h, inner.width, list_h);
    let editor = Rect::new(inner.x, list.y + list_h, inner.width, editor_h);
    let footer = Rect::new(inner.x, editor.y + editor_h, inner.width, footer_h);
    [header, list, editor, footer]
}

/// Computes the dialog frame, the reviewer-row area, and the action buttons
/// from the same layout the renderer uses.
pub fn review_dialog_geometry(term: Rect, entry_count: usize) -> ReviewDialogGeometry {
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
    let chunks = review_dialog_chunks(area, entry_count);
    let model_rows = inset(chunks[1]);

    let footer = chunks[3];
    let confirm_w = REVIEW_DIALOG_CONFIRM_LABEL.len() as u16 + 2;
    let cancel_w = REVIEW_DIALOG_CANCEL_LABEL.len() as u16 + 2;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = footer.x + (footer.width.saturating_sub(total)) / 2;
    let confirm_button = Rect::new(start_x, footer.y, confirm_w, 1);
    let cancel_button = Rect::new(start_x + confirm_w + gap, footer.y, cancel_w, 1);

    ReviewDialogGeometry {
        area,
        model_rows,
        confirm_button,
        cancel_button,
    }
}

pub fn render_review_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::ReviewDialogState,
    primary: PrimaryModifier,
) {
    let entry_count = dialog.selector.len();
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Launch Code Review ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    frame.render_widget(outer_block, area);

    let [header_area, list_area, editor_area, footer_area] =
        review_dialog_chunks(area, entry_count);

    let dim = Style::default().fg(Color::DarkGray);

    // Header: what is about to be reviewed.
    let header_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Run automated code review on active context files?",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Files: ", dim),
            Span::styled(
                dialog.files_count.to_string(),
                Style::default().fg(Color::White).bold(),
            ),
            Span::styled("    Estimated tokens: ", dim),
            Span::styled(
                format!("~{}", dialog.estimated_tokens),
                Style::default().fg(Color::White).bold(),
            ),
        ]),
        Line::from(Span::styled(
            "  Only file contents are sent: history and repo map are excluded.",
            dim,
        )),
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
        format!(
            " Reviewer Model  [{} of {}] ",
            dialog.selector.selected_index() + 1,
            entry_count
        )
    } else {
        " Reviewer Model ".to_string()
    };
    let list_border = if dialog.focus.is_model_list() {
        Style::default().fg(Color::Magenta).bold()
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(list_border);
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    if entry_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (no reviewer models configured)",
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = dialog.selector.render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Instruction editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = dialog.focus.is_prompt_editor();
    let mut editor_text = vec![Line::from(Span::styled(
        "  Additional review instructions (optional):",
        Style::default().fg(Color::Cyan).bold(),
    ))];
    if dialog.prompt_editor.is_empty() {
        let mut spans = vec![Span::raw("  > ")];
        if show_cursor {
            spans.push(Span::styled("█", Style::default().fg(Color::Yellow)));
        }
        spans.push(Span::styled(
            " (type additional review focus or leave empty)...",
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
    let confirm_selected = dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm;
    let confirm_style = if confirm_selected {
        Style::default().bg(Color::Green).fg(Color::Black).bold()
    } else {
        Style::default().fg(Color::Green)
    };
    let cancel_style = if confirm_selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().bg(Color::Cyan).fg(Color::Black).bold()
    };
    let nav_hint = if dialog.focus.is_actions() {
        "Enter: confirm • ←/→: button • Esc: cancel".to_string()
    } else {
        format!(
            "Tab: focus • {}: model • Esc: cancel",
            crate::components::model_selector::model_select_hint(primary)
        )
    };
    let actions = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", REVIEW_DIALOG_CONFIRM_LABEL), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" {} ", REVIEW_DIALOG_CANCEL_LABEL), cancel_style),
        ]),
        Line::from(Span::styled(nav_hint, Style::default().fg(Color::DarkGray))),
    ];
    let footer = Paragraph::new(actions).alignment(Alignment::Center);
    frame.render_widget(footer, footer_area);
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
    }
}
