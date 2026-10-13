use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;
use super::{editor_lines, render_modal_frame, ModalFrameParams};

pub fn render_prompt_modal(
    frame: &mut ratatui::Frame,
    modal: &crate::app::PromptModalState,
) {
    let area = centered_rect(68, 56, frame.area());
    let (dialog_title, border_color, header_lines) = match &modal.target {
        crate::app::PromptModalTarget::Review { review_id, review_title, focused_items } => {
            let title = if let Some((item_id, _)) = focused_items.first() {
                format!("Discuss Finding: [{}] #{}", review_id, item_id)
            } else {
                format!("Discuss Review Session: [{}]", review_id)
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" Review: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(review_title, Style::default().bold().fg(Color::White)),
                ]),
            ];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Entire review session", Style::default().fg(Color::Yellow).bold()),
                    Span::styled(" (discussing code quality findings and resolution plan)", Style::default().fg(Color::DarkGray)),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(" Target finding: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        truncate_to_width(&format!("#{} {}", id, title), max_w),
                        Style::default().bold().fg(Color::Green),
                    ),
                ]));
            }
            (title, Color::Magenta, lines)
        }
        crate::app::PromptModalTarget::Plan { plan_id, plan_title, focused_items } => {
            let title = if let Some((step_id, _)) = focused_items.first() {
                format!("Discuss Step: [{}] #{}", plan_id, step_id)
            } else {
                format!("Discuss Plan: [{}]", plan_id)
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" Plan: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(plan_title, Style::default().bold().fg(Color::White)),
                ]),
            ];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" Scope: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Entire plan", Style::default().fg(Color::Yellow).bold()),
                    Span::styled(" (discussing architecture and overall strategy)", Style::default().fg(Color::DarkGray)),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(" Target step: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        truncate_to_width(&format!("#{} {}", id, title), max_w),
                        Style::default().bold().fg(Color::Green),
                    ),
                ]));
            }
            (title, Color::Cyan, lines)
        }
    };

    let is_actions_focused = modal.focus.is_actions();
    let frame_params = ModalFrameParams {
        title: &dialog_title,
        border_color,
        area,
        confirm_label: "Send to AI (Enter)",
        cancel_label: "Cancel (Esc)",
        actions_state: modal.actions,
        is_actions_focused,
        destructive: false,
        hint_text: if is_actions_focused {
            Some("← / → switch button • Enter/Space execute • Tab to Editor • y / n fast select • Esc cancel")
        } else {
            Some("Type message • Enter send • Shift+Enter / Alt+Enter / C-J newline • Tab to Buttons • Esc cancel")
        },
    };

    render_modal_frame(frame, frame_params, |f, body_area| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_lines.len() as u16 + 1),
                Constraint::Min(4),
            ])
            .split(body_area);

        f.render_widget(Paragraph::new(header_lines), chunks[0]);

        let is_editor_focused = modal.focus.is_editor();
        let mut editor_content = Vec::new();
        if modal.prompt_editor.is_empty() {
            let cursor = if is_editor_focused { "█" } else { " " };
            editor_content.push(Line::from(vec![
                Span::styled(cursor, Style::default().fg(Color::Yellow)),
                Span::styled(
                    " Enter your comments, questions, or directives for Tauqe AI...",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        } else {
            editor_content.extend(editor_lines(&modal.prompt_editor, is_editor_focused));
        }

        let editor_border_color = if is_editor_focused {
            Color::Green
        } else {
            Color::DarkGray
        };

        let editor_block = Block::default()
            .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J newline) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(editor_border_color));
        let editor_widget = Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false });
        f.render_widget(editor_widget, chunks[1]);
    });
}
