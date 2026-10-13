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

pub fn render_status_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::StatusDialogState,
) {
    let options_count = match &dialog.target {
        crate::app::StatusDialogTarget::PlanItem { .. } => 6,
        crate::app::StatusDialogTarget::ReviewItem { .. } => 5,
    };
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
                tauqe_protocol::ReviewStatus::Rejected => 4,
            };
            let opts = vec![
                ("DISCUSSION", Color::Magenta, Color::Black, "Under review / requires discussion"),
                ("TODO", Color::Yellow, Color::Black, "Approved / ready for implementation"),
                ("IN_PROGRESS", Color::Cyan, Color::Black, "Currently being fixed"),
                ("FIXED", Color::Green, Color::Black, "Resolved and addressed in code"),
                ("REJECTED", Color::DarkGray, Color::White, "False positive / won't fix"),
            ];
            (" Change Finding Status ", item_title.as_str(), cur, opts)
        }
    };

    let outer_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(options_count as u16),
            Constraint::Length(3),
        ])
        .split(inner_area);

    let max_title_w = chunks[0].width.saturating_sub(10) as usize;
    let target_preview = truncate_to_width(raw_target_title.trim(), max_title_w);
    let target_line = Line::from(vec![
        Span::styled(" Item: ", Style::default().fg(Color::DarkGray)),
        Span::styled(target_preview, Style::default().bold().fg(Color::White)),
    ]);
    frame.render_widget(Paragraph::new(vec![Line::raw(""), target_line, Line::raw("")]), chunks[0]);

    let mut body_lines = Vec::new();
    for (idx, (label, bg, fg, desc)) in options.iter().enumerate() {
        let is_cursor = idx == dialog.selected_index;
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

    let fast_select_label = if options_count >= 6 {
        "1-6"
    } else if options_count >= 5 {
        "1-5"
    } else {
        "1-4"
    };
    let actions = vec![
        Line::from(vec![
            Span::styled(format!(" [ Apply (Enter / {}) ] ", fast_select_label), Style::default().bg(Color::Green).fg(Color::Black).bold()),
            Span::raw("    "),
            Span::styled(" [ Cancel (Esc) ] ", Style::default().bg(Color::DarkGray).fg(Color::White)),
        ]),
        Line::from(Span::styled(
            format!("{} fast select • n/p or j/k navigate • Enter apply • Esc cancel", fast_select_label),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let actions_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[2]);
}
