use ratatui::layout::{Constraint, Direction, Layout};

use crate::app::{AppState, ViewMode};

pub mod context;
pub mod develop;
pub mod dialogs;
pub mod footer;
pub mod geometry;
pub mod header;
pub mod history;
pub mod onboarding;
pub mod review;
pub mod wrap;

use context::render_context_view;
use develop::render::render_develop_view;
use history::render_history_view;
use review::render_review_view;

pub use dialogs::{
    render_confirm_cancel_popup, render_confirm_clear_history_popup, render_confirm_undo_popup,
    render_help_popup, render_review_dialog, render_selection_dialog, render_squash_popup,
};
pub use footer::{extract_current_round, format_footer_cost, render_footer};
pub use geometry::centered_rect;
pub use header::render_header;
pub use onboarding::{format_masked_key, render_onboarding_view};
pub use wrap::{wrap_line, wrap_lines};

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    let term_height = frame.area().height;
    let term_width = frame.area().width;
    let max_input_height = (term_height * 4 / 10).clamp(6, 16);
    let inner_input_width = (term_width.saturating_sub(2)) as usize;
    let (total_visual_lines, cursor_visual_line) = state
        .input_editor
        .visual_lines_and_cursor(inner_input_width);

    let needed_input_height = (total_visual_lines as u16 + 2).max(3);
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
            render_develop_view(frame, state, chunks[1], chunks[2], cursor_visual_line);
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
    }

    if state.view_mode != ViewMode::Onboarding {
        render_footer(frame, state, chunks[3]);
    }

    if let Some(dialog) = &state.squash_dialog {
        render_squash_popup(frame, dialog);
    } else if let Some(dialog) = &state.review_dialog {
        render_review_dialog(frame, dialog);
    } else if state.confirm_cancel {
        render_confirm_cancel_popup(frame, state);
    } else if state.confirm_undo {
        render_confirm_undo_popup(frame, state);
    } else if state.confirm_clear_history {
        render_confirm_clear_history_popup(frame, state);
    } else if let Some(dialog) = &state.selection_dialog {
        render_selection_dialog(frame, dialog);
    } else if state.show_help {
        render_help_popup(frame, state.view_mode);
    }
}
