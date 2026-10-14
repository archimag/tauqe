use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::config::PrimaryModifier;
use crate::ui::centered_rect;
use super::editor_lines;

/// Width of the review launch dialog in columns.
pub const REVIEW_DIALOG_WIDTH: u16 = 64;
/// Height of the review launch dialog in rows.
pub const REVIEW_DIALOG_HEIGHT: u16 = 54;
/// Action-bar label for starting a review.
pub const REVIEW_DIALOG_CONFIRM_LABEL: &str = "[ Start Review ]";
/// Action-bar label for cancelling the review dialog.
pub const REVIEW_DIALOG_CANCEL_LABEL: &str = "[ Cancel ]";

/// Computes the dialog frame and action-button rectangles from the same
/// constants and labels the renderer uses. Keeping a single source of truth
/// guarantees mouse hit-testing never drifts from what is drawn.
pub fn review_dialog_geometry(term: Rect) -> (Rect, Rect, Rect) {
    let area = centered_rect(REVIEW_DIALOG_WIDTH, REVIEW_DIALOG_HEIGHT, term);
    let inner_y = area.y + 1;
    let inner_h = area.height.saturating_sub(2);
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);
    let btn_y = inner_y + inner_h.saturating_sub(3);
    let confirm_w = REVIEW_DIALOG_CONFIRM_LABEL.len() as u16 + 2;
    let cancel_w = REVIEW_DIALOG_CANCEL_LABEL.len() as u16 + 2;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = inner_x + (inner_w.saturating_sub(total)) / 2;
    let confirm_btn = Rect::new(start_x, btn_y, confirm_w, 1);
    let cancel_btn = Rect::new(start_x + confirm_w + gap, btn_y, cancel_w, 1);
    (area, confirm_btn, cancel_btn)
}

pub fn render_review_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::ReviewDialogState,
    primary: PrimaryModifier,
) {
    let area = review_dialog_geometry(frame.area()).0;
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Launch Code Review ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let dim = Style::default().fg(Color::DarkGray);

    let mut lines = vec![
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
    ];

    // Single source of truth for the reviewer presentation: the shared
    // ModelSelector stepper renders the role badge, the selected entry's
    // display label, its reasoning effort, and the [X of Y] counter (c-2).
    let mut model_spans = vec![Span::styled("  Reviewer: ", dim)];
    model_spans.extend(dialog.selector.render_stepper_spans(primary));
    lines.push(Line::from(model_spans));

    lines.push(Line::from(Span::styled(
        "  Only file contents are sent: history and repo map are excluded.",
        dim,
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Additional review instructions (optional):",
        Style::default().fg(Color::Cyan).bold(),
    )));

    if dialog.prompt_editor.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("  > "),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " (type additional review focus or leave empty)...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        for l in editor_lines(&dialog.prompt_editor, true) {
            let mut spans = vec![Span::raw("  > ")];
            spans.extend(l.spans);
            lines.push(Line::from(spans));
        }
    }

    let body = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(body, chunks[0]);

    let confirm_focused = dialog.confirm_button == crate::app::ConfirmDialogButton::Confirm;
    let confirm_style = if confirm_focused {
        Style::default().bg(Color::Green).fg(Color::Black).bold()
    } else {
        Style::default().fg(Color::Green)
    };
    let cancel_style = if confirm_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().bg(Color::Cyan).fg(Color::Black).bold()
    };
    let nav_hint = if dialog.selector.len() > 1 {
        "Tab / ←/→ focus • C-M switch model • Enter confirm • Esc cancel"
    } else {
        "Tab / ←/→ focus • Enter confirm • Esc cancel"
    };
    let actions = vec![
        Line::from(vec![
            Span::styled(format!(" {} ", REVIEW_DIALOG_CONFIRM_LABEL), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" {} ", REVIEW_DIALOG_CANCEL_LABEL), cancel_style),
        ]),
        Line::from(Span::styled(
            nav_hint,
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_dialog_geometry_buttons_within_area() {
        let term = Rect::new(0, 0, 120, 40);
        let (area, confirm_btn, cancel_btn) = review_dialog_geometry(term);
        assert!(area.width > 0 && area.height > 0);
        assert!(confirm_btn.width > 0 && cancel_btn.width > 0);
        assert_eq!(confirm_btn.y, cancel_btn.y);
        assert!(confirm_btn.right() <= area.right());
        assert!(cancel_btn.right() <= area.right());
        assert!(confirm_btn.right() < cancel_btn.x);
    }
}
