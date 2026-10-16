use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{ActiveModal, AppState, ViewMode};
use crate::theme::engine::current_engine;

pub mod footer;
pub mod geometry;
pub mod header;
pub mod wrap;

use crate::dialogs::render::{
    render_confirm_batch_cancel_popup, render_confirm_cancel_popup,
    render_confirm_clear_auto_popup, render_confirm_clear_history_popup,
    render_confirm_clear_user_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_detach_focus_popup,
    render_confirm_execute_review_popup, render_confirm_execute_scope_popup,
    render_confirm_quit_popup, render_confirm_undo_popup, render_disconnected_popup,
    render_help_popup, render_plan_refine_blocked_dialog, render_plan_refine_dialog,
    render_prompt_modal, render_review_dialog, render_selection_dialog, render_squash_popup,
    render_status_dialog,
};
use crate::views::context::render_context_view;
use crate::views::develop::render::render_develop_view;
use crate::views::history::render_history_view;
use crate::views::onboarding::render_onboarding_view;
use crate::views::plans::render_plans_view;
use crate::views::review::render_review_view;

pub use footer::{extract_current_round, format_footer_cost, render_footer};
pub use geometry::centered_rect;
pub use header::render_header;
pub use wrap::{wrap_line, wrap_lines};

const MIN_TERM_WIDTH: u16 = 60;
const MIN_TERM_HEIGHT: u16 = 12;
/// On terminals shorter than this the footer and the non-Develop info strip
/// are dropped so the main area keeps enough rows to stay usable.
const COMPACT_TERM_HEIGHT: u16 = 20;

/// Solid background filler that unconditionally sets symbol and background color
/// for every grid cell, preventing unstyled gaps and terminal default background leakage.
struct CanvasBackground(ratatui::style::Color);

impl ratatui::widgets::Widget for CanvasBackground {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                cell.set_symbol(" ");
                cell.set_bg(self.0);
            }
        }
    }
}

fn render_terminal_too_small(frame: &mut ratatui::Frame) {
    let engine = current_engine();
    let area = frame.area();
    let lines = vec![
        Line::from(Span::styled(
            "Terminal too small",
            engine.fg(engine.palette().status_warning).bold(),
        )),
        Line::from(format!("Current: {}x{}", area.width, area.height)),
        Line::from(format!("Required: {}x{} (Ctrl+Q to exit)", MIN_TERM_WIDTH, MIN_TERM_HEIGHT)),
    ];
    let height = area.height.min(3);
    let rect = Rect::new(area.x, area.y + area.height.saturating_sub(height) / 2, area.width, height);
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}

pub fn render_ui(frame: &mut ratatui::Frame, state: &mut AppState) {
    let area = frame.area();
    state.last_frame_area = area;
    if area.width < MIN_TERM_WIDTH || area.height < MIN_TERM_HEIGHT {
        render_terminal_too_small(frame);
        return;
    }
    let engine = current_engine();
    let palette = engine.palette();

    // Synchronize host terminal window margins with the theme canvas.
    crate::terminal::update_terminal_background(palette.canvas);

    // Paint the theme canvas across the entire grid buffer so every cell is initialized
    // with the active palette background, eliminating terminal color bars.
    frame.render_widget(CanvasBackground(engine.color(palette.canvas)), area);

    let term_height = area.height;
    let term_width = area.width;
    let compact = term_height < COMPACT_TERM_HEIGHT;

    // The bottom area is the prompt editor in Develop and a fixed telemetry
    // strip elsewhere. Only Develop sizes it from the prompt draft, so a draft
    // never inflates an unrelated view's info row (Conventions.md §2).
    let is_busy = state.model.is_busy();
    let (prompt_lines, cursor_visual_line, bottom_height) = if state.view_mode == ViewMode::Develop {
        let max_input_height = (term_height * 4 / 10).clamp(6, 16);
        let inner_input_width = (term_width.saturating_sub(2)) as usize;
        let (lines, cursor) = state.input_editor.build_prompt_lines(inner_input_width, is_busy);
        let height = (lines.len() as u16 + 2).max(3).min(max_input_height);
        (lines, cursor, height)
    } else if compact {
        (Vec::new(), 0, 0)
    } else {
        (Vec::new(), 0, 3)
    };

    // Degrade gracefully on short terminals instead of refusing to render.
    let footer_height = if compact { 0 } else { 3 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(bottom_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

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

    // Render exactly the modal the canonical ordering reports as top-most, so the
    // visible window is always the window that receives input.
    match state.active_modal() {
        Some(ActiveModal::ServerDisconnected) => {
            if let Some(msg) = &state.server_disconnected {
                render_disconnected_popup(frame, msg);
            }
        }
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = &mut state.squash_dialog {
                render_squash_popup(frame, dialog);
            }
        }
        Some(ActiveModal::Review) => {
            if let Some(dialog) = &state.review_dialog {
                render_review_dialog(frame, dialog, state.tui_config.input.primary_modifier);
            }
        }
        Some(ActiveModal::ConfirmCancel) => render_confirm_cancel_popup(frame, state),
        Some(ActiveModal::ConfirmQuit) => render_confirm_quit_popup(frame, state),
        Some(ActiveModal::BatchCancel) => render_confirm_batch_cancel_popup(frame, state),
        Some(ActiveModal::ClearAuto) => render_confirm_clear_auto_popup(frame, state),
        Some(ActiveModal::ClearUser) => render_confirm_clear_user_popup(frame, state),
        Some(ActiveModal::ConfirmUndo) => render_confirm_undo_popup(frame, state),
        Some(ActiveModal::ConfirmDetachFocus) => {
            render_confirm_detach_focus_popup(frame, state)
        }
        Some(ActiveModal::DeletePlan) => {
            if let Some(plan_id) = &state.confirm_delete_plan {
                render_confirm_delete_plan_popup(frame, plan_id, state);
            }
        }
        Some(ActiveModal::DeleteReview) => {
            if let Some(session_id) = &state.confirm_delete_review {
                render_confirm_delete_review_popup(frame, session_id, state);
            }
        }
        Some(ActiveModal::ExecuteReview) => render_confirm_execute_review_popup(frame, state),
        Some(ActiveModal::ExecuteScope) => render_confirm_execute_scope_popup(frame, state),
        Some(ActiveModal::RefineDialog) => {
            if let Some(dialog) = &state.plans_view.refine_dialog {
                render_plan_refine_dialog(frame, dialog, state.tui_config.input.primary_modifier);
            }
        }
        Some(ActiveModal::RefineBlocked) => {
            if let Some(blocked) = &state.plans_view.refine_blocked {
                render_plan_refine_blocked_dialog(frame, blocked, state.confirm_button);
            }
        }
        Some(ActiveModal::Prompt) => {
            if let Some(modal) = &state.prompt_modal {
                render_prompt_modal(frame, modal);
            }
        }
        Some(ActiveModal::ClearHistory) => render_confirm_clear_history_popup(frame, state),
        Some(ActiveModal::Status) => {
            if let Some(dialog) = &state.status_dialog {
                render_status_dialog(frame, dialog);
            }
        }
        Some(ActiveModal::Selection) => {
            if let Some(dialog) = &state.selection_dialog {
                render_selection_dialog(frame, dialog);
            }
        }
        Some(ActiveModal::ThemeSelector) => {
            if let Some(dialog) = &state.theme_selector {
                crate::theme::selector::render_theme_selector(frame, dialog);
            }
        }
        Some(ActiveModal::Help) => {
            render_help_popup(
                frame,
                state.view_mode,
                &mut state.help_scroll,
                state.tui_config.input.primary_modifier,
            );
        }
        None => {}
    }
}
