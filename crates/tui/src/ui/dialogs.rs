use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{ConfirmDialogButton, ModalActionsState};

pub use crate::app::{ModalActionsState as ExportedModalActionsState, ModalFocus as ExportedModalFocus};
use crate::editor::InputEditor;

use super::centered_rect;

pub mod confirm;
pub mod disconnected;
pub mod discuss;
pub mod help;
pub mod prompt;
pub mod refine;
pub mod selection;
pub mod squash;
pub mod status;

pub use confirm::{
    render_confirm_cancel_popup, render_confirm_clear_auto_popup,
    render_confirm_clear_history_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_quit_popup, render_confirm_undo_popup,
};
pub use disconnected::render_disconnected_popup;
pub use discuss::render_discuss_plan_dialog;
pub use help::{get_mode_key_commands, render_help_popup, GLOBAL_COMMANDS, ONBOARDING_COMMANDS};
pub use prompt::render_prompt_modal;
pub use refine::{render_plan_refine_blocked_dialog, render_plan_refine_dialog};
pub use selection::{
    render_selection_dialog, selection_dialog_area, selection_dialog_visible_height,
};
pub use squash::{
    render_squash_confirm_popup, render_squash_discard_popup, render_squash_popup,
    squash_dialog_area, squash_dialog_chunks,
};
pub use status::{render_status_dialog, status_dialog_area};

pub struct ConfirmModalParams<'a> {
    pub title: &'a str,
    pub border_color: Color,
    pub body_lines: Vec<Line<'a>>,
    pub confirm_label: &'a str,
    pub cancel_label: &'a str,
    pub confirm_button: crate::app::ConfirmDialogButton,
    pub destructive: bool,
    pub width: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfirmModalGeometry {
    pub area: Rect,
    pub confirm_button: Rect,
    pub cancel_button: Rect,
}

/// Computes the exact geometry and button bounds for three-tier confirmation modals,
/// shared between renderer and mouse hit-testing.
pub fn confirm_modal_geometry(
    term: Rect,
    width: u16,
    body_lines_count: usize,
    confirm_label: &str,
    cancel_label: &str,
) -> ConfirmModalGeometry {
    let needed_height = (body_lines_count as u16 + 7).clamp(10, 22);
    let height_percent = (needed_height * 100 / term.height.max(1)).clamp(25, 75);
    let area = centered_rect(width, height_percent, term);

    let inner_y = area.y + 1;
    let inner_h = area.height.saturating_sub(2);
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);

    let actions_y = inner_y + inner_h.saturating_sub(3);
    let btn_y = actions_y;

    let l1 = confirm_label.chars().count() + 6; // " [ " + label + " ] "
    let l2 = cancel_label.chars().count() + 6;  // " [ " + label + " ] "
    let total_len = l1 + 4 + l2;                 // 4 spaces gap
    let start_x = inner_x + (inner_w.saturating_sub(total_len as u16)) / 2;

    ConfirmModalGeometry {
        area,
        confirm_button: Rect::new(start_x, btn_y, l1 as u16, 1),
        cancel_button: Rect::new(start_x + l1 as u16 + 4, btn_y, l2 as u16, 1),
    }
}

pub struct ModalFrameParams<'a> {
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

/// Standardized three-tier modal frame renderer (Header, Body, Actions).
pub fn render_modal_frame<F>(
    frame: &mut ratatui::Frame,
    params: ModalFrameParams,
    render_body: F,
) where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    frame.render_widget(Clear, params.area);

    let outer_block = Block::default()
        .title(format!(" {} ", params.title))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(params.border_color));
    let inner_area = outer_block.inner(params.area);
    frame.render_widget(outer_block, params.area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(inner_area);

    render_body(frame, chunks[0]);

    let (confirm_style, cancel_style) = if params.is_actions_focused {
        match params.actions_state.focused_button {
            ConfirmDialogButton::Confirm => {
                let bg = if params.destructive { Color::Red } else { Color::Green };
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
            Style::default().bg(Color::DarkGray).fg(Color::Gray),
            Style::default().bg(Color::DarkGray).fg(Color::Gray),
        )
    };

    let hint = params.hint_text.unwrap_or(
        "Tab / ← / → focus • Enter/Space execute • y / n fast select • Esc cancel",
    );

    let actions_lines = vec![
        Line::from(vec![
            Span::styled(format!(" [ {} ] ", params.confirm_label), confirm_style),
            Span::raw("    "),
            Span::styled(format!(" [ {} ] ", params.cancel_label), cancel_style),
        ]),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ];
    let actions_widget = Paragraph::new(actions_lines).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(actions_widget, chunks[1]);
}

/// Standardized three-tier confirmation modal renderer (Header, Body, Actions).
pub fn render_modal_confirm(frame: &mut ratatui::Frame, params: ConfirmModalParams) {
    let geom = confirm_modal_geometry(
        frame.area(),
        params.width,
        params.body_lines.len(),
        params.confirm_label,
        params.cancel_label,
    );

    let frame_params = ModalFrameParams {
        title: params.title,
        border_color: params.border_color,
        area: geom.area,
        confirm_label: params.confirm_label,
        cancel_label: params.cancel_label,
        actions_state: params.confirm_button.into(),
        is_actions_focused: true,
        destructive: params.destructive,
        hint_text: Some("Tab / ← / → focus • Enter/Space execute • y / n fast select • Esc cancel"),
    };

    render_modal_frame(frame, frame_params, |f, body_area| {
        let body_widget = Paragraph::new(params.body_lines)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: false });
        f.render_widget(body_widget, body_area);
    });
}

/// Renders editor text as lines, drawing the cursor (reversed char or a block at line end).
pub(super) fn editor_lines(editor: &InputEditor, show_cursor: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for raw in editor.get_text().split('\n') {
        let end = start + raw.len();
        if show_cursor && editor.cursor >= start && editor.cursor <= end {
            let (before, rest) = raw.split_at(editor.cursor - start);
            let mut spans = vec![Span::raw(before.to_string())];
            let mut chars = rest.chars();
            match chars.next() {
                Some(c) => {
                    spans.push(Span::styled(c.to_string(), Style::default().reversed()));
                    spans.push(Span::raw(chars.as_str().to_string()));
                }
                None => spans.push(Span::styled("█", Style::default().fg(Color::Yellow))),
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::raw(raw.to_string()));
        }
        start = end + 1;
    }
    lines
}

    
    
    
pub fn render_review_dialog(frame: &mut ratatui::Frame, dialog: &crate::app::ReviewDialogState) {
    let area = centered_rect(64, 54, frame.area());
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

    let model = dialog
        .models
        .get(dialog.model_index)
        .map(|m| m.to_string())
        .unwrap_or_else(|| "(none)".to_string());
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
        Line::from(vec![
            Span::styled("  Model: ", dim),
            Span::styled(
                format!("◄ {} ►", model),
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled("  (Tab / Shift+Tab to switch)", dim),
        ]),
        Line::from(Span::styled(
            "  Only file contents are sent: history and repo map are excluded.",
            dim,
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "  Additional review instructions (optional):",
            Style::default().fg(Color::Cyan).bold(),
        )),
    ];

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

    let actions = vec![
        Line::from(vec![
            Span::styled(
                " [ Start Review (Enter) ] ",
                Style::default().bg(Color::Green).fg(Color::Black).bold(),
            ),
            Span::raw("    "),
            Span::styled(
                " [ Cancel (Esc) ] ",
                Style::default().bg(Color::DarkGray).fg(Color::White),
            ),
        ]),
        Line::from(Span::styled(
            "Enter start review • Esc cancel • Tab / Shift+Tab or ↑/↓ switch model",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let footer = Paragraph::new(actions).alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer, chunks[1]);
}

    