use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::{editor_lines, render_modal_frame, ModalFrameParams, ModalTone};
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::centered_rect;
use crate::ui::header::truncate_to_width;

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
        &active_t("modal.send"),
        &active_t("modal.cancel"),
    )
}

pub fn render_prompt_modal(frame: &mut ratatui::Frame, modal: &crate::app::PromptModalState) {
    let engine = current_engine();
    let palette = engine.palette();
    let area = prompt_modal_area(frame.area());
    let (dialog_title, tone, header_lines) = match &modal.target {
        crate::app::PromptModalTarget::Review {
            review_id,
            review_title,
            focused_items,
        } => {
            let title = if let Some((item_id, _)) = focused_items.first() {
                active_t("dialog.prompt.title_finding")
                    .replacen("{}", review_id, 1)
                    .replacen("{}", &item_id.to_string(), 1)
            } else {
                active_t("dialog.prompt.title_review").replacen("{}", review_id, 1)
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    active_t("dialog.prompt.review_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(review_title, engine.fg(palette.text_primary).bold()),
            ])];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(
                        active_t("dialog.prompt.scope_label"),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        active_t("dialog.prompt.scope_review"),
                        engine.fg(palette.status_warning).bold(),
                    ),
                    Span::styled(
                        active_t("dialog.prompt.scope_review_note"),
                        engine.fg(palette.text_muted),
                    ),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(
                        active_t("dialog.prompt.target_finding_label"),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        truncate_to_width(&format!("#{} {}", id, title), max_w),
                        engine.fg(palette.status_success).bold(),
                    ),
                ]));
            }
            (title, ModalTone::Accent, lines)
        }
        crate::app::PromptModalTarget::Plan {
            plan_id,
            plan_title,
            focused_items,
        } => {
            let title = if let Some((step_id, _)) = focused_items.first() {
                active_t("dialog.prompt.title_step")
                    .replacen("{}", plan_id, 1)
                    .replacen("{}", &step_id.to_string(), 1)
            } else {
                active_t("dialog.prompt.title_plan").replacen("{}", plan_id, 1)
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    active_t("dialog.prompt.plan_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(plan_title, engine.fg(palette.text_primary).bold()),
            ])];
            if focused_items.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(
                        active_t("dialog.prompt.scope_label"),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        active_t("dialog.prompt.scope_plan"),
                        engine.fg(palette.status_warning).bold(),
                    ),
                    Span::styled(
                        active_t("dialog.prompt.scope_plan_note"),
                        engine.fg(palette.text_muted),
                    ),
                ]));
            } else if let Some((id, title)) = focused_items.first() {
                let max_w = area.width.saturating_sub(22) as usize;
                lines.push(Line::from(vec![
                    Span::styled(
                        active_t("dialog.prompt.target_step_label"),
                        engine.fg(palette.text_muted),
                    ),
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
    let confirm_label = active_t("modal.send");
    let cancel_label = active_t("modal.cancel");
    // The footer vocabulary is projected through the shared semantic span
    // renderer: every group reads `CHORD / CHORD verb`, so chords are
    // emphasized, verbs read as descriptions, and separators recede.
    let hint = if is_actions_focused {
        active_t("dialog.prompt.hint_actions")
    } else {
        active_t("dialog.prompt.hint_editor")
    };
    let frame_params = ModalFrameParams {
        title: &dialog_title,
        tone,
        area,
        confirm_label: &confirm_label,
        cancel_label: &cancel_label,
        actions_state: modal.actions,
        is_actions_focused,
        destructive: false,
        hint_text: Some(&hint),
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
                    format!(" {}", active_t("dialog.prompt.editor_placeholder")),
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
            .title(format!(" {} ", active_t("dialog.prompt.editor_title")))
            .borders(Borders::ALL)
            .border_style(editor_border_style)
            .style(Style::default().bg(engine.color(palette.surface)));
        let editor_widget = Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false });
        f.render_widget(editor_widget, chunks[1]);
    });
}
