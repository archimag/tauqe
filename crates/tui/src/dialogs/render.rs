use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::modal_geometry;
use crate::theme::engine::current_engine;

pub(crate) use crate::components::confirm::{
    confirm_modal_geometry, render_modal_confirm, ConfirmModalGeometry, ConfirmModalParams,
};
pub(crate) use crate::components::modal::{render_modal_frame, ModalFrameParams, ModalTone};
use crate::editor::InputEditor;

pub(crate) mod batch_cancel;
pub(crate) mod confirm;
pub(crate) mod disconnected;
pub(crate) mod execute_review;
pub(crate) mod help;
pub(crate) mod locale;
pub(crate) mod prompt;
pub(crate) mod refine;
pub(crate) mod review;
pub(crate) mod selection;
pub(crate) mod squash;
pub(crate) mod status;
pub(crate) mod usage;

pub(crate) use batch_cancel::{batch_cancel_modal_geometry, render_confirm_batch_cancel_popup};
pub(crate) use confirm::{
    clear_auto_params, clear_history_params, clear_horizon_final_params, clear_horizon_params,
    clear_user_params, confirm_cancel_params, confirm_detach_focus_params, confirm_quit_params,
    delete_plan_final_params, delete_plan_params, delete_review_params, execute_pipeline_params,
    execute_review_params, execute_scope_params, plan_unfinished_steps,
    render_confirm_cancel_popup, render_confirm_clear_auto_popup,
    render_confirm_clear_history_popup, render_confirm_clear_horizon_final_popup,
    render_confirm_clear_horizon_popup, render_confirm_clear_user_popup,
    render_confirm_delete_plan_final_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_detach_focus_popup,
    render_confirm_execute_pipeline_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_quit_popup,
};
pub(crate) use disconnected::{disconnected_button_geometry, render_disconnected_popup};
pub(crate) use execute_review::execute_review_dialog_geometry;
pub(crate) use help::render_help_popup;
pub(crate) use locale::{locale_dialog_geometry, render_locale_dialog};
pub(crate) use prompt::{prompt_modal_area, prompt_modal_buttons, render_prompt_modal};
pub(crate) use refine::{
    refine_blocked_params, refine_dialog_geometry, render_plan_refine_blocked_dialog,
    render_plan_refine_dialog,
};
pub(crate) use review::{render_review_dialog, review_dialog_geometry};
pub(crate) use selection::{
    render_selection_dialog, selection_dialog_area, selection_dialog_visible_height,
};
pub(crate) use squash::{render_squash_popup, squash_dialog_chunks};
pub(crate) use status::{
    render_status_dialog, status_dialog_area, status_dialog_buttons, status_dialog_options_area,
};
pub(crate) use usage::{render_usage_summary, usage_summary_area};

/// Renders editor text as lines, drawing the cursor (reversed char or a block at line end).
pub(super) fn editor_lines(editor: &InputEditor, show_cursor: bool) -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
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
                None => spans.push(Span::styled("█", engine.fg(palette.status_warning))),
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::raw(raw.to_string()));
        }
        start = end + 1;
    }
    lines
}

/// Shared chrome metrics of a model-list + instructions-editor dialog.
///
/// Every picker dialog — Architect refinement, review launch, review execution —
/// stacks the same sections, so the minimum editor height, the model-row cap,
/// the optional target-field height, and the inter-section spacing live in one
/// place and can never drift between the three surfaces (Conventions §1.3).
const PICKER_DIALOG_EDITOR_MIN_HEIGHT: u16 = 3;
const PICKER_DIALOG_MODEL_LIST_MAX_ROWS: u16 = 6;
/// One blank row separating two heterogeneous sections of a picker dialog, so
/// the model list, the target field, and the editor never read as a single
/// continuous block (Conventions §6.7).
const PICKER_DIALOG_SPACER_HEIGHT: u16 = 1;
/// Reserved height of the optional single-line target field that sits between
/// the model list and the instructions editor.
pub(super) const PICKER_DIALOG_PATH_HEIGHT: u16 = 3;

/// Vertical sections of a picker dialog interior, stacked from the top.
pub(super) struct PickerDialogChunks {
    /// Header block (dialog subject and metadata).
    pub(super) header: Rect,
    /// Model list block, one row per entry before borders.
    pub(super) list: Rect,
    /// Optional single-line target field; zero-height when the dialog has none.
    pub(super) path: Rect,
    /// Instructions / directives editor block.
    pub(super) editor: Rect,
    /// Action bar holding the confirm and cancel buttons.
    pub(super) buttons: Rect,
    /// One-row docked hint strip pinned to the card's bottom edge.
    pub(super) footer: Rect,
}

/// Insets a rectangle by a one-cell border on every side.
pub(super) fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Number of model rows drawn for `entry_count` entries, before borders.
fn picker_model_list_rows(entry_count: usize) -> u16 {
    (entry_count.max(1) as u16).min(PICKER_DIALOG_MODEL_LIST_MAX_ROWS)
}

/// Splits a picker dialog card into `[header, list, path, editor, buttons,
/// footer]`.
///
/// The whole card chrome — the one-row top inset under the border title, the
/// one-column margin on each side, the pre-action spacer, and the docked hint
/// strip — comes from the shared [`modal_geometry`] formula, so a picker breathes
/// exactly like every other modal. Inside that air-aware body the sections are
/// stacked in visual order and separated by one blank row each, so the model
/// list, the target field, and the instructions editor read as three distinct
/// zones instead of one continuous block (Conventions §6.7, §6.8).
///
/// The list, target field, and header are sized first so the dialog stays usable
/// on short terminals; the instructions editor absorbs whatever height remains.
pub(super) fn picker_dialog_chunks(
    area: Rect,
    entry_count: usize,
    header_height: u16,
    path_height: u16,
) -> PickerDialogChunks {
    let geometry = modal_geometry(area);
    let body = geometry.body;

    // The list never takes more than half of the body, so the header, the target
    // field, and the editors always keep space on short terminals.
    let list_h = (picker_model_list_rows(entry_count) + 2).min(body.height / 2);

    let mut remaining = body.height;
    let mut cursor = body.y;

    // Header block, capped so its spacer and the minimal editor always survive.
    let header_h = header_height.min(
        remaining.saturating_sub(PICKER_DIALOG_EDITOR_MIN_HEIGHT + PICKER_DIALOG_SPACER_HEIGHT),
    );
    let header = Rect::new(body.x, cursor, body.width, header_h);
    cursor = cursor.saturating_add(header_h);
    remaining = remaining.saturating_sub(header_h);

    // Inter-section spacer under the header.
    let header_gap = PICKER_DIALOG_SPACER_HEIGHT.min(remaining);
    cursor = cursor.saturating_add(header_gap);
    remaining = remaining.saturating_sub(header_gap);

    let list_h = list_h.min(remaining);
    let list = Rect::new(body.x, cursor, body.width, list_h);
    cursor = cursor.saturating_add(list_h);
    remaining = remaining.saturating_sub(list_h);

    // The optional target field is reached through its own spacer, so a dialog
    // without one never inherits a stray blank row above the editor.
    let path_gap = if path_height == 0 {
        0
    } else {
        PICKER_DIALOG_SPACER_HEIGHT.min(remaining)
    };
    cursor = cursor.saturating_add(path_gap);
    remaining = remaining.saturating_sub(path_gap);

    let path_h = if path_height == 0 {
        0
    } else {
        path_height.min(remaining.saturating_sub(PICKER_DIALOG_EDITOR_MIN_HEIGHT))
    };
    let path = Rect::new(body.x, cursor, body.width, path_h);
    cursor = cursor.saturating_add(path_h);
    remaining = remaining.saturating_sub(path_h);

    // Instructions editor, reached through its own spacer.
    let editor_gap = PICKER_DIALOG_SPACER_HEIGHT.min(remaining);
    cursor = cursor.saturating_add(editor_gap);
    remaining = remaining.saturating_sub(editor_gap);

    let editor = Rect::new(body.x, cursor, body.width, remaining);

    PickerDialogChunks {
        header,
        list,
        path,
        editor,
        buttons: geometry.action_bar,
        footer: geometry.hint_row,
    }
}

/// Centers the confirm and cancel buttons on the action bar, measuring the drawn
/// localized labels so the clickable buttons always match the rendered ones
/// (Conventions §6, §7).
pub(super) fn picker_button_rects(
    buttons: Rect,
    confirm_label: &str,
    cancel_label: &str,
) -> (Rect, Rect) {
    let confirm_w = UnicodeWidthStr::width(confirm_label) as u16;
    let cancel_w = UnicodeWidthStr::width(cancel_label) as u16;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = buttons.x + (buttons.width.saturating_sub(total)) / 2;
    let confirm_button = Rect::new(start_x, buttons.y, confirm_w, 1);
    let cancel_button = Rect::new(start_x + confirm_w + gap, buttons.y, cancel_w, 1);
    (confirm_button, cancel_button)
}
