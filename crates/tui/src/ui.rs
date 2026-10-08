use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{AppState, ViewMode};

pub mod context;
pub mod develop;
pub mod dialogs;
pub mod footer;
pub mod geometry;
pub mod header;
pub mod history;
pub mod onboarding;
pub mod plans;
pub mod review;
pub mod wrap;

use context::render_context_view;
use develop::render::render_develop_view;
use history::render_history_view;
use plans::render_plans_view;
use review::render_review_view;

pub use dialogs::{
    render_confirm_cancel_popup, render_confirm_clear_auto_popup,
    render_confirm_clear_history_popup, render_confirm_delete_plan_popup,
    render_confirm_quit_popup, render_confirm_undo_popup,
    render_disconnected_popup, render_help_popup, render_review_dialog, render_selection_dialog,
    render_squash_popup, selection_dialog_area,
};
pub use footer::{extract_current_round, format_footer_cost, render_footer};
pub use geometry::centered_rect;
pub use header::render_header;
pub use onboarding::{format_masked_key, render_onboarding_view};
pub use wrap::{wrap_line, wrap_lines};

const MIN_TERM_WIDTH: u16 = 80;
const MIN_TERM_HEIGHT: u16 = 24;

fn render_terminal_too_small(frame: &mut ratatui::Frame) {
    let area = frame.area();
    let lines = vec![
        Line::from(Span::styled(
            "Terminal too small",
            Style::default().fg(Color::Yellow).bold(),
        )),
        Line::from(format!("Current: {}x{}", area.width, area.height)),
        Line::from(format!("Required: {}x{} (Ctrl+Q to exit)", MIN_TERM_WIDTH, MIN_TERM_HEIGHT)),
    ];
    let height = area.height.min(3);
    let rect = Rect::new(area.x, area.y + area.height.saturating_sub(height) / 2, area.width, height);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    if frame.area().width < MIN_TERM_WIDTH || frame.area().height < MIN_TERM_HEIGHT {
        render_terminal_too_small(frame);
        return;
    }
    let term_height = frame.area().height;
    let term_width = frame.area().width;
    let max_input_height = (term_height * 4 / 10).clamp(6, 16);
    let inner_input_width = (term_width.saturating_sub(2)) as usize;
    let (prompt_lines, cursor_visual_line) = crate::ui::develop::render::build_prompt_lines(
        &state.input_editor,
        inner_input_width,
        state.model.is_busy(),
    );

    let needed_input_height = (prompt_lines.len() as u16 + 2).max(3);
    let input_height = needed_input_height.min(max_input_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(input_height),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_header(frame, state, chunks[0]);

    match state.view_mode {
        ViewMode::Onboarding => {
            render_onboarding_view(frame, state, chunks[1], chunks[2], chunks[3]);
        }
        ViewMode::Develop => {
            render_develop_view(
                frame,
                state,
                chunks[1],
                chunks[2],
                prompt_lines,
                cursor_visual_line,
            );
        }
        ViewMode::Context => {
            render_context_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::History => {
            render_history_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::Review => {
            render_review_view(frame, state, chunks[1], chunks[2]);
        }
        ViewMode::Plans => {
            render_plans_view(frame, state, chunks[1], chunks[2]);
        }
    }

    if state.view_mode != ViewMode::Onboarding {
        render_footer(frame, state, chunks[3]);
    }

    if let Some(msg) = &state.server_disconnected {
        render_disconnected_popup(frame, msg);
    } else if let Some(dialog) = &mut state.squash_dialog {
        render_squash_popup(frame, dialog);
    } else if let Some(dialog) = &state.review_dialog {
        render_review_dialog(frame, dialog);
    } else if state.confirm_cancel {
        render_confirm_cancel_popup(frame, state);
    } else if state.confirm_quit {
        render_confirm_quit_popup(frame, state);
    } else if state.confirm_undo {
        render_confirm_undo_popup(frame, state);
    } else if state.confirm_clear_history {
        render_confirm_clear_history_popup(frame, state);
    } else if state.context_view.confirm_clear_auto {
        render_confirm_clear_auto_popup(frame, state);
    } else if let Some(plan_id) = &state.confirm_delete_plan {
        render_confirm_delete_plan_popup(frame, plan_id, state);
    } else if let Some(dialog) = &state.selection_dialog {
        render_selection_dialog(frame, dialog);
    } else if state.show_help {
        render_help_popup(
            frame,
            state.view_mode,
            &mut state.help_scroll,
            state.tui_config.input.primary_modifier,
        );
    }
}
