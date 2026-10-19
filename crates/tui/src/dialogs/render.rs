use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::theme::engine::current_engine;

pub(crate) use crate::components::confirm::{
    confirm_modal_geometry, render_modal_confirm, ConfirmModalGeometry, ConfirmModalParams,
};
pub(crate) use crate::components::modal::{render_modal_frame, ModalFrameParams, ModalTone};
use crate::editor::InputEditor;

pub(crate) mod confirm;
pub(crate) mod disconnected;
pub(crate) mod help;
pub(crate) mod locale;
pub(crate) mod prompt;
pub(crate) mod refine;
pub(crate) mod review;
pub(crate) mod selection;
pub(crate) mod squash;
pub(crate) mod status;
pub(crate) mod usage;

pub(crate) use confirm::{
    batch_cancel_modal_geometry, clear_auto_params, clear_history_params, clear_horizon_final_params,
    clear_horizon_params, clear_user_params, confirm_cancel_params, confirm_detach_focus_params,
    confirm_quit_params, delete_plan_params, delete_review_params, execute_review_dialog_geometry,
    execute_pipeline_params, execute_review_params, execute_scope_params,
    render_confirm_batch_cancel_popup, render_confirm_cancel_popup,
    render_confirm_clear_auto_popup, render_confirm_clear_history_popup,
    render_confirm_clear_horizon_final_popup, render_confirm_clear_horizon_popup,
    render_confirm_clear_user_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_detach_focus_popup,
    render_confirm_execute_pipeline_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_quit_popup,
};
pub(crate) use disconnected::{disconnected_button_geometry, render_disconnected_popup};
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
/// stacks the same sections, so the footer height, the minimum editor height,
/// the model-row cap, and the optional target-field height live in one place and
/// can never drift between the three surfaces (Conventions §1.3).
const PICKER_DIALOG_FOOTER_HEIGHT: u16 = 3;
const PICKER_DIALOG_EDITOR_MIN_HEIGHT: u16 = 3;
const PICKER_DIALOG_MODEL_LIST_MAX_ROWS: u16 = 6;
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

/// Splits a picker dialog interior into `[header, list, path, editor, buttons,
/// footer]`.
///
/// The footer, list, target field, and header are sized first so the dialog stays
/// usable on short terminals; the instructions editor absorbs whatever height
/// remains. The footer splits into the interactive action bar and a one-row docked
/// hint strip pinned to the card's bottom edge.
pub(super) fn picker_dialog_chunks(
    area: Rect,
    entry_count: usize,
    header_height: u16,
    path_height: u16,
) -> PickerDialogChunks {
    let inner = inset(area);

    let footer_h = PICKER_DIALOG_FOOTER_HEIGHT.min(inner.height);
    let after_footer = inner.height - footer_h;

    // The list never takes more than half of the room left after the footer, so
    // the header and editors always keep space on short terminals.
    let list_h = (picker_model_list_rows(entry_count) + 2).min(after_footer / 2);
    let after_list = after_footer - list_h;

    let path_h = path_height.min(after_list.saturating_sub(PICKER_DIALOG_EDITOR_MIN_HEIGHT));
    let after_path = after_list - path_h;

    let header_h = header_height.min(after_path.saturating_sub(PICKER_DIALOG_EDITOR_MIN_HEIGHT));
    let editor_h = after_path - header_h;

    let header = Rect::new(inner.x, inner.y, inner.width, header_h);
    let list = Rect::new(inner.x, inner.y + header_h, inner.width, list_h);
    let path = Rect::new(inner.x, list.y + list_h, inner.width, path_h);
    let editor = Rect::new(inner.x, path.y + path_h, inner.width, editor_h);

    // Split the footer into the action buttons and the docked hint strip so the
    // hint reads as persistent chrome rather than a second row of buttons.
    let docked_h = 1.min(footer_h);
    let buttons_h = footer_h - docked_h;
    let buttons = Rect::new(inner.x, editor.y + editor_h, inner.width, buttons_h);
    let footer = Rect::new(inner.x, buttons.y + buttons_h, inner.width, docked_h);

    PickerDialogChunks {
        header,
        list,
        path,
        editor,
        buttons,
        footer,
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
