use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

pub(crate) use crate::components::confirm::{
    confirm_modal_geometry, render_modal_confirm, ConfirmModalGeometry, ConfirmModalParams,
};
pub(crate) use crate::components::modal::{render_modal_frame, ModalFrameParams};
use crate::editor::InputEditor;

pub(crate) mod confirm;
pub(crate) mod disconnected;
pub(crate) mod discuss;
pub(crate) mod help;
pub(crate) mod prompt;
pub(crate) mod refine;
pub(crate) mod review;
pub(crate) mod selection;
pub(crate) mod squash;
pub(crate) mod status;

pub(crate) use confirm::{
    clear_auto_params, clear_history_params, clear_user_params, confirm_cancel_params,
    confirm_detach_focus_params, confirm_quit_params, confirm_undo_params, delete_plan_params,
    delete_review_params, execute_review_params, execute_scope_params,
    render_confirm_cancel_popup, render_confirm_clear_auto_popup,
    render_confirm_clear_history_popup, render_confirm_clear_user_popup,
    render_confirm_delete_plan_popup, render_confirm_delete_review_popup,
    render_confirm_detach_focus_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_quit_popup, render_confirm_undo_popup,
};
pub(crate) use disconnected::render_disconnected_popup;
pub(crate) use discuss::{
    discuss_plan_dialog_area, discuss_plan_dialog_buttons, render_discuss_plan_dialog,
};
pub(crate) use help::render_help_popup;
pub(crate) use prompt::render_prompt_modal;
pub(crate) use refine::{
    refine_dialog_geometry, render_plan_refine_blocked_dialog, render_plan_refine_dialog,
};
pub(crate) use review::{render_review_dialog, review_dialog_geometry};
pub(crate) use selection::{
    render_selection_dialog, selection_dialog_area, selection_dialog_visible_height,
};
pub(crate) use squash::{render_squash_popup, squash_dialog_chunks};
pub(crate) use status::{render_status_dialog, status_dialog_area};

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
