use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::super::centered_rect;

pub fn render_disconnected_popup(frame: &mut ratatui::Frame, message: &str) {
    let area = centered_rect(64, 30, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Server Disconnected ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(3)])
        .split(inner_area);

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Connection to tauqe-server was terminated",
            Style::default().bold().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(Span::styled(message, Style::default().fg(Color::White))),
    ];
    let body = Paragraph::new(lines)
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, chunks[0]);

    let actions = vec![
        Line::from(Span::styled(
            " [ Quit TAUQE (q / Ctrl+Q) ] ",
            Style::default().bg(Color::Red).fg(Color::White).bold(),
        )),
        Line::from(Span::styled(
            "Press 'q', 'Esc' or Ctrl+Q to exit",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}
