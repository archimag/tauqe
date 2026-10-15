//! Reusable three-tier modal frame: header/title, body, and actions footer.
//!
//! Every modal dialog shares this frame. It renders the bordered container,
//! splits it into a flexible body and a fixed actions bar, and styles the
//! confirm/cancel buttons according to the focused button and whether the action
//! is destructive. Views supply only the body content through a closure, keeping
//! the dialog chrome in one place (Conventions.md §7).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::{ConfirmDialogButton, ModalActionsState};

/// Parameters for the standardized three-tier modal frame.
pub struct ModalFrame<'a> {
    pub title: &'a str,
    pub border_color: Color,
    pub area: Rect,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub actions_state: ModalActionsState,
    pub is_actions_focused: bool,
    pub destructive: bool,
    pub hint_text: Option<&'a str>,
}

/// Historical alias for [`ModalFrame`] kept for existing call sites.
pub type ModalFrameParams<'a> = ModalFrame<'a>;

impl ModalFrame<'_> {
    /// Renders the frame, delegating the central body area to `render_body`.
    pub fn render<F>(&self, frame: &mut ratatui::Frame, render_body: F)
    where
        F: FnOnce(&mut ratatui::Frame, Rect),
    {
        frame.render_widget(Clear, self.area);

        let outer_block = Block::default()
            .title(format!(" {} ", self.title))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.border_color));
        let inner_area = outer_block.inner(self.area);
        frame.render_widget(outer_block, self.area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(2), Constraint::Length(3)])
            .split(inner_area);

        render_body(frame, chunks[0]);

        let (confirm_style, cancel_style) = if self.is_actions_focused {
            match self.actions_state.focused_button {
                ConfirmDialogButton::Confirm => {
                    let bg = if self.destructive {
                        Color::Red
                    } else {
                        Color::Green
                    };
                    (
                        Style::default().bg(bg).fg(Color::White).bold(),
                        Style::default().bg(Color::DarkGray).fg(Color::White),
                    )
                }
                ConfirmDialogButton::Cancel => (
                    Style::default().bg(Color::DarkGray).fg(Color::White),
                    Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
                ),
            }
        } else {
            (
                Style::default().bg(Color::DarkGray).fg(Color::White),
                Style::default().bg(Color::DarkGray).fg(Color::White),
            )
        };

        let hint = self
            .hint_text
            .unwrap_or("Tab / ← / → focus • Enter/Space execute • y / n fast select • Esc cancel");

        let actions_lines = vec![
            Line::from(vec![
                Span::styled(format!(" [ {} ] ", self.confirm_label), confirm_style),
                Span::raw("    "),
                Span::styled(format!(" [ {} ] ", self.cancel_label), cancel_style),
            ]),
            Line::from(Span::styled(hint, Style::default().fg(Color::Gray))),
        ];
        let actions_widget = Paragraph::new(actions_lines).alignment(Alignment::Center);
        frame.render_widget(actions_widget, chunks[1]);
    }
}

/// Free-function form of [`ModalFrame::render`] for views that build the
/// parameter bundle inline.
pub fn render_modal_frame<F>(frame: &mut ratatui::Frame, params: ModalFrame<'_>, render_body: F)
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    params.render(frame, render_body);
}
