use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::theme::engine::current_engine;
use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;
use super::{editor_lines, render_modal_frame, ModalFrameParams, ModalTone};

/// Geometry of the discussion prompt modal, shared by rendering and mouse
/// hit-testing so the drawn buttons are always the clickable ones
/// (Conventions §6, §7).
pub fn prompt_modal_area(term: Rect) -> Rect {
    centered_rect(68, 56, term)
}

/// Confirm/cancel button rectangles of the prompt modal.
pub fn prompt_modal_buttons(term: Rect) -> (Rect, Rect) {
    crate::components::modal::modal_action_buttons(
        prompt_modal_area(term),
        "Send to AI (Enter)",
        "Cancel (Esc)",
    )
}

pub fn render_prompt_modal(
    frame: &mut ratatui::Frame,
    modal: &crate::app::PromptModalState,
) {
    let engine = current_engine();
    let palette = engine.palette();
    let area = prompt_modal_area(frame.area());
    let (dialog_title, tone, header_lines) = match &modal.target {
        crate::app::PromptModalTarget::Review { review_id, review_title, focused_items } => {
            let title = if let Some((item_id, _)) = focused_items.first() {
                format!("Discuss Finding: [{}] #{}", review_id, item_id)
            } else {
                format!("Discuss Review Session: [{}]", review_id)
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" Review: ", engine.fg(palette.text_muted)),
                    Span::styled(review_title, engine.fg(palette.text_primary).bold()),
                ]),
            ];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" Scope: ", engine.fg(palette.text_muted)),
                    Span::styled("Entire review session", engine.fg(palette.status_warning).bold()),
                    Span::styled(" (discussing code quality findings and resolution plan)", engine.fg(palette.text_muted)),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(" Target finding: ", engine.fg(palette.text_muted)),
                    Span::styled(
                        truncate_to_width(&format!("#{} {}", id, title), max_w),
                        engine.fg(palette.status_success).bold(),
                    ),
                ]));
            }
            (title, ModalTone::Accent, lines)
        }
        crate::app::PromptModalTarget::Plan { plan_id, plan_title, focused_items } => {
            let title = if let Some((step_id, _)) = focused_items.first() {
                format!("Discuss Step: [{}] #{}", plan_id, step_id)
            } else {
                format!("Discuss Plan: [{}]", plan_id)
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" Plan: ", engine.fg(palette.text_muted)),
                    Span::styled(plan_title, engine.fg(palette.text_primary).bold()),
                ]),
            ];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" Scope: ", engine.fg(palette.text_muted)),
                    Span::styled("Entire plan", engine.fg(palette.status_warning).bold()),
                    Span::styled(" (discussing architecture and overall strategy)", engine.fg(palette.text_muted)),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(" Target step: ", engine.fg(palette.text_muted)),
                    Span::styled(
                        truncate_to_width(&format!("#{} {}", id, title), max_w),
                        engine.fg(palette.status_success).bold(),
                    ),
                ]));
            }
            (title, ModalTone::Info, lines)
        }
    };

    let is_actions_focused = modal.focus.is_actions();
    let frame_params = ModalFrameParams {
        title: &dialog_title,
        tone,
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
                Span::styled(cursor, engine.fg(palette.status_warning)),
                Span::styled(
                    " Enter your comments, questions, or directives for Tauqe AI...",
                    engine.fg(palette.text_muted),
                ),
            ]));
        } else {
            editor_content.extend(editor_lines(&modal.prompt_editor, is_editor_focused));
        }

        let editor_border_style = if is_editor_focused {
            engine.fg(palette.status_success).bold()
        } else {
            engine.fg(palette.border)
        };

        let editor_block = Block::default()
            .title(" Your Comments / Questions (Enter send, Shift+Enter / Alt+Enter / C-J newline) ")
            .borders(Borders::ALL)
            .border_style(editor_border_style)
            .style(Style::default().bg(engine.color(palette.surface)));
        let editor_widget = Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false });
        f.render_widget(editor_widget, chunks[1]);
    });
}
