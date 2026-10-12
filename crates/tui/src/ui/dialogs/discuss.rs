use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::super::centered_rect;
use super::super::header::truncate_to_width;
use super::editor_lines;

pub fn render_discuss_plan_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::DiscussPlanDialogState,
) {
    let area = centered_rect(68, 56, frame.area());
    frame.render_widget(Clear, area);

    let dialog_title = if let Some((step_id, _)) = dialog.focused_items.first() {
        format!(" Discuss Step: [{}] #{} ", dialog.plan_id, step_id)
    } else {
        format!(" Discuss Plan: [{}] ", dialog.plan_id)
    };

    let outer_block = Block::default()
        .title(dialog_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(" Plan: ", Style::default().fg(Color::DarkGray)),
            Span::styled(&dialog.plan_title, Style::default().bold().fg(Color::White)),
        ]),
    ];

    if dialog.focused_items.is_empty() {
        header_lines.push(Line::from(vec![
            Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
            Span::styled("Entire plan", Style::default().fg(Color::Yellow).bold()),
            Span::styled(" (discussing architecture and overall strategy)", Style::default().fg(Color::DarkGray)),
        ]));
    } else if let Some((id, title)) = dialog.focused_items.first() {
        header_lines.push(Line::from(vec![
            Span::styled(" Target step: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate_to_width(&format!("#{} {}", id, title), chunks[0].width.saturating_sub(18) as usize),
                Style::default().bold().fg(Color::Green),
            ),
        ]));
    }

    let header_widget = Paragraph::new(header_lines);
    frame.render_widget(header_widget, chunks[0]);

    let mut editor_content = Vec::new();
    if dialog.prompt_editor.is_empty() {
        editor_content.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " Enter your architectural questions, task critique, or refinement proposals for Tauqe AI...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        editor_content.extend(editor_lines(&dialog.prompt_editor, true));
    }

    let editor_block = Block::default()
        .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J / C-N newline) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(
                " [ Discuss in Develop (Enter / C-Enter) ] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw("    "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter send • Shift+Enter / Alt+Enter / C-J / C-N newline • Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}
