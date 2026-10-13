use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;
use super::editor_lines;

/// Confirm button label as drawn by [`render_discuss_plan_dialog`], including
/// its surrounding `[ ]` decoration.
const DISCUSS_CONFIRM_LABEL: &str = " [ Discuss in Develop (Enter / C-Enter) ] ";
/// Cancel button label decoration.
const DISCUSS_CANCEL_LABEL: &str = " [ Cancel (Esc) ] ";
/// Spacing drawn between the two action buttons.
const DISCUSS_BUTTON_GAP: &str = "    ";

/// Single source of truth for the discuss dialog geometry, shared by the
/// renderer and mouse hit-testing.
pub fn discuss_plan_dialog_area(term: Rect) -> Rect {
    centered_rect(68, 56, term)
}

/// Returns the confirm and cancel button rectangles of the discuss dialog,
/// reproducing the centered action bar drawn by [`render_discuss_plan_dialog`]
/// so mouse hit-testing cannot drift from the rendered buttons.
pub fn discuss_plan_dialog_buttons(term: Rect) -> (Rect, Rect) {
    let area = discuss_plan_dialog_area(term);
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);
    let inner_y = area.y + 1;
    let inner_h = area.height.saturating_sub(2);
    let btn_y = inner_y + inner_h.saturating_sub(3);

    let confirm_w = DISCUSS_CONFIRM_LABEL.chars().count() as u16;
    let cancel_w = DISCUSS_CANCEL_LABEL.chars().count() as u16;
    let gap_w = DISCUSS_BUTTON_GAP.chars().count() as u16;
    let total = confirm_w + gap_w + cancel_w;
    let start_x = inner_x + inner_w.saturating_sub(total) / 2;

    (
        Rect::new(start_x, btn_y, confirm_w, 1),
        Rect::new(start_x + confirm_w + gap_w, btn_y, cancel_w, 1),
    )
}

pub fn render_discuss_plan_dialog(
    frame: &mut ratatui::Frame,
    dialog: &crate::app::DiscussPlanDialogState,
) {
    let area = discuss_plan_dialog_area(frame.area());
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
        .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J newline) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));
    let editor_widget = Paragraph::new(editor_content).block(editor_block).wrap(Wrap { trim: false });
    frame.render_widget(editor_widget, chunks[1]);

    let actions = vec![
        Line::from(vec![
            Span::styled(
                DISCUSS_CONFIRM_LABEL,
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw(DISCUSS_BUTTON_GAP),
            Span::styled(
                DISCUSS_CANCEL_LABEL,
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter send • Shift+Enter / Alt+Enter / C-J newline • Esc cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer_widget = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Mouse hit-testing must compute the same button rectangles the renderer
    /// draws for the discuss dialog action bar.
    #[test]
    fn discuss_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let dialog = crate::app::DiscussPlanDialogState {
            plan_id: "plan-1".to_string(),
            plan_title: "Test Plan".to_string(),
            focused_items: Vec::new(),
            prompt_editor: crate::editor::InputEditor::default(),
        };

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_discuss_plan_dialog(f, &dialog))
            .unwrap();

        let (confirm_btn, cancel_btn) = discuss_plan_dialog_buttons(term);
        let buffer = terminal.backend().buffer();

        let confirm_text: String = (confirm_btn.x..confirm_btn.x + confirm_btn.width)
            .map(|x| buffer[(x, confirm_btn.y)].symbol())
            .collect();
        assert!(
            confirm_text.contains("Discuss"),
            "confirm row text: {confirm_text:?}"
        );

        let cancel_text: String = (cancel_btn.x..cancel_btn.x + cancel_btn.width)
            .map(|x| buffer[(x, cancel_btn.y)].symbol())
            .collect();
        assert!(
            cancel_text.contains("Cancel"),
            "cancel row text: {cancel_text:?}"
        );
    }
}
