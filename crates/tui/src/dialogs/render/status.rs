use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;

pub fn status_dialog_area(term: Rect, options_count: usize) -> Rect {
    let height = (options_count as u16 + 9).clamp(11, 18);
    let height_percent = (height * 100 / term.height.max(1)).clamp(30, 65);
    centered_rect(60, height_percent, term)
}

/// Fast-select hint rendered on the Apply button, matching the `1..N` digits
/// the dialog accepts for the current option count.
fn status_fast_select_label(options_count: usize) -> &'static str {
    if options_count >= 6 {
        "1-6"
    } else if options_count >= 5 {
        "1-5"
    } else {
        "1-4"
    }
}

/// Apply button label as drawn by [`render_status_dialog`], including its
/// surrounding `[ ]` decoration.
fn status_apply_label(options_count: usize) -> String {
    format!(
        " [ Apply (Enter / {}) ] ",
        status_fast_select_label(options_count)
    )
}

/// Cancel button label decoration.
const STATUS_CANCEL_LABEL: &str = " [ Cancel (Esc) ] ";
/// Spacing drawn between the two action buttons.
const STATUS_BUTTON_GAP: &str = "    ";

/// Splits the dialog interior into `[header, options, footer]`.
///
/// Shared by the renderer and mouse hit-testing so the option rows and the
/// action buttons stay in lockstep and can never drift apart (Conventions §2).
fn status_dialog_chunks(area: Rect, options_count: usize) -> [Rect; 3] {
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(options_count as u16),
            Constraint::Length(3),
        ])
        .split(inner);
    [chunks[0], chunks[1], chunks[2]]
}

/// Returns the apply and cancel button rectangles of the status dialog,
/// reproducing the centered action bar drawn by [`render_status_dialog`] so
/// mouse hit-testing cannot drift from the rendered buttons (Conventions §6).
pub fn status_dialog_buttons(term: Rect, options_count: usize) -> (Rect, Rect) {
    let area = status_dialog_area(term, options_count);
    let footer = status_dialog_chunks(area, options_count)[2];

    let apply_w = status_apply_label(options_count).chars().count() as u16;
    let cancel_w = STATUS_CANCEL_LABEL.chars().count() as u16;
    let gap_w = STATUS_BUTTON_GAP.chars().count() as u16;
    let total = apply_w + gap_w + cancel_w;
    let start_x = footer.x + footer.width.saturating_sub(total) / 2;

    (
        Rect::new(start_x, footer.y, apply_w, 1),
        Rect::new(start_x + apply_w + gap_w, footer.y, cancel_w, 1),
    )
}

pub fn render_status_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::StatusDialogState,
) {
    let options_count = dialog.target.options_count();
    let area = status_dialog_area(frame.area(), options_count);
    frame.render_widget(Clear, area);

    let (title, raw_target_title, current_idx, options) = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::PlanItemStatus::Discussion => 0,
                tauqe_protocol::PlanItemStatus::Todo => 1,
                tauqe_protocol::PlanItemStatus::InProgress => 2,
                tauqe_protocol::PlanItemStatus::Done => 3,
                tauqe_protocol::PlanItemStatus::Blocked => 4,
                tauqe_protocol::PlanItemStatus::Cancelled => 5,
            };
            let opts = vec![
                ("DISCUSSION", Color::Magenta, Color::Black, "Under review / requires discussion"),
                ("TODO", Color::Yellow, Color::Black, "Approved / ready for execution"),
                ("IN_PROGRESS", Color::Cyan, Color::Black, "Currently in active development"),
                ("DONE", Color::Green, Color::Black, "Completed and verified"),
                ("BLOCKED", Color::Red, Color::White, "Blocked by unmet prerequisites / needs replanning"),
                ("CANCELLED", Color::DarkGray, Color::White, "Superseded or cancelled"),
            ];
            (" Change Task Status ", item_title.as_str(), cur, opts)
        }
        crate::app::StatusDialogTarget::ReviewItem { item_title, current_status, .. } => {
            let cur = match current_status {
                tauqe_protocol::ReviewStatus::Discussion => 0,
                tauqe_protocol::ReviewStatus::Todo => 1,
                tauqe_protocol::ReviewStatus::InProgress => 2,
                tauqe_protocol::ReviewStatus::Fixed => 3,
                tauqe_protocol::ReviewStatus::Skipped => 4,
                tauqe_protocol::ReviewStatus::Rejected => 5,
            };
            let opts = vec![
                ("DISCUSSION", Color::Magenta, Color::Black, "Under review / requires discussion"),
                ("TODO", Color::Yellow, Color::Black, "Approved / ready for implementation"),
                ("IN_PROGRESS", Color::Cyan, Color::Black, "Currently being fixed"),
                ("FIXED", Color::Green, Color::Black, "Resolved and addressed in code"),
                ("SKIPPED", Color::DarkGray, Color::White, "Skipped / no code changes needed"),
                ("REJECTED", Color::DarkGray, Color::White, "False positive / won't fix"),
            ];
            (" Change Finding Status ", item_title.as_str(), cur, opts)
        }
    };

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    frame.render_widget(outer_block, area);

    let chunks = status_dialog_chunks(area, options_count);

    let max_title_w = chunks[0].width.saturating_sub(10) as usize;
    let target_preview = truncate_to_width(raw_target_title.trim(), max_title_w);
    let target_line = Line::from(vec![
        Span::styled(" Item: ", Style::default().fg(Color::DarkGray)),
        Span::styled(target_preview, Style::default().bold().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::raw(""), target_line, Line::raw("")]), chunks[0]);

    let mut body_lines = Vec::new();
    for (idx, (label, bg, fg, desc)) in options.iter().enumerate() {
        let is_cursor = idx == dialog.selector.selected_index();
        let is_current = idx == current_idx;

        let cursor_prefix = if is_cursor { " ▶ " } else { "   " };
        let current_marker = if is_current { "(•) " } else { "( ) " };
        let key_shortcut = format!("[{}] ", idx + 1);

        let row_style = if is_cursor {
            Style::default().bg(Color::DarkGray).fg(Color::White).bold()
        } else {
            Style::default()
        };

        let badge_style = Style::default().bg(*bg).fg(*fg).bold();

        body_lines.push(Line::from(vec![
            Span::styled(cursor_prefix, Style::default().fg(Color::Cyan).bold()),
            Span::styled(
                current_marker,
                if is_current {
                    Style::default().fg(Color::Green).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(key_shortcut, Style::default().fg(Color::Yellow).bold()),
            Span::styled(format!(" {:<11} ", label), badge_style),
            Span::raw(" "),
            Span::styled(*desc, if is_cursor { Style::default().fg(Color::White) } else { Style::default().fg(Color::Gray) }),
        ]).style(row_style));
    }
    frame.render_widget(Paragraph::new(body_lines), chunks[1]);

    let fast_select_label = status_fast_select_label(options_count);
    let actions = vec![
        Line::from(vec![
            Span::styled(
                status_apply_label(options_count),
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw(STATUS_BUTTON_GAP),
            Span::styled(
                STATUS_CANCEL_LABEL,
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            format!("{} fast select • n/p or j/k navigate • Enter apply • Esc cancel", fast_select_label),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Mouse hit-testing must compute the same button rectangles the renderer
    /// draws for the status dialog action bar, so a click on Apply resolves to
    /// Apply instead of falling through to the dismiss path.
    #[test]
    fn status_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: tauqe_protocol::PlanItemStatus::Todo,
            },
            0,
        );
        let options_count = dialog.target.options_count();

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_status_dialog(f, &dialog))
            .unwrap();

        let (apply_btn, cancel_btn) = status_dialog_buttons(term, options_count);
        let buffer = terminal.backend().buffer();

        let apply_text: String = (apply_btn.x..apply_btn.x + apply_btn.width)
            .map(|x| buffer[(x, apply_btn.y)].symbol())
            .collect();
        assert!(
            apply_text.contains("Apply"),
            "apply row text: {apply_text:?}"
        );

        let cancel_text: String = (cancel_btn.x..cancel_btn.x + cancel_btn.width)
            .map(|x| buffer[(x, cancel_btn.y)].symbol())
            .collect();
        assert!(
            cancel_text.contains("Cancel"),
            "cancel row text: {cancel_text:?}"
        );

        assert!(
            apply_btn.right() <= cancel_btn.x,
            "action buttons must not overlap"
        );
    }
}
