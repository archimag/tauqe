//! Rendering for the AI architect plan refinement dialog and its blocked-state
//! popup.

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::ui::centered_rect;
use super::{editor_lines, render_modal_confirm, ConfirmModalParams};

pub fn render_plan_refine_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::views::plans::PlanRefineDialogState,
) {
    let area = centered_rect(68, 56, frame.area());
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(" Deep Plan Refinement (AI Architect) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner_area = outer_block.inner(area);
    frame.render_widget(outer_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(3)])
        .split(inner_area);

    let (model_label, _) = dialog
        .models
        .get(dialog.selected_model_index)
        .cloned()
        .unwrap_or_else(|| ("Default".to_string(), None));
    let dim = Style::default().fg(Color::DarkGray);

    let header_lines = vec![
        Line::from(vec![
            Span::styled(" Plan: ", dim),
            Span::styled(&dialog.plan_title, Style::default().bold().fg(Color::White)),
            Span::styled(format!(" [{}]", dialog.plan_id), dim),
        ]),
        Line::from(vec![
            Span::styled(" Model: ", dim),
            Span::styled(
                format!("◄ {} ►", model_label),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled("  (Tab / Shift+Tab to switch)", dim),
        ]),
        Line::from(Span::styled(
            " Decomposes high-level tasks into verified atomic engineering steps without altering agreed architecture.",
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(header_lines), chunks[0]);

    let mut editor_content = Vec::new();
    if dialog.instructions_editor.is_empty() {
        editor_content.push(Line::from(vec![
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                " Additional refinement directives (e.g. 'split step 2 into atomic subtasks') [optional]...",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    } else {
        editor_content.extend(editor_lines(&dialog.instructions_editor, true));
    }

    let editor_block = Block::default()
        .title(" Architect Instructions (optional) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let (confirm_style, cancel_style) = match dialog.confirm_button {
        crate::app::ConfirmDialogButton::Confirm => (
            Style::default().bg(Color::Green).fg(Color::Black).bold(),
            Style::default().bg(Color::DarkGray).fg(Color::White),
        ),
        crate::app::ConfirmDialogButton::Cancel => (
            Style::default().bg(Color::DarkGray).fg(Color::White),
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
    };

    let actions = vec![
        Line::from(vec![
            Span::styled(" [ Start Refinement ] ", confirm_style),
            Span::raw("    "),
            Span::styled(" [ Cancel ] ", cancel_style),
        ]),
        Line::from(Span::styled(
            "Tab / ← / → switch button • Enter execute focused • Esc cancel • ↑ / ↓ switch model",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}

pub fn render_plan_refine_blocked_dialog(
    frame: &mut ratatui::Frame,
    blocked: &crate::views::plans::PlanRefineBlockedState,
) {
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "AI Architect Refused Decomposition (Blockers Detected)",
            Style::default().bold().fg(Color::Yellow),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("Plan: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} [{}]", blocked.plan_title, blocked.plan_id),
                Style::default().bold().fg(Color::White),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled("Reason / Contradiction:", Style::default().fg(Color::Red).bold())),
    ];

    for line in blocked.reason.lines() {
        body_lines.push(Line::from(Span::styled(format!("  {}", line), Style::default().fg(Color::White))));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE prohibits speculative guessing. Resolve architectural blockers with AI in Develop.",
        Style::default().fg(Color::DarkGray),
    )));

    render_modal_confirm(
        frame,
        ConfirmModalParams {
            title: "Plan Refinement Blocked",
            border_color: Color::Red,
            body_lines,
            confirm_label: "Discuss in Develop (D)",
            cancel_label: "Dismiss (Esc)",
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            destructive: false,
            width: 70,
        },
    );
}
