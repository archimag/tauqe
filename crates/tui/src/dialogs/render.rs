use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::components::modal::modal_geometry;
use crate::theme::engine::current_engine;

pub(crate) use crate::components::confirm::{
    confirm_modal_geometry, render_modal_confirm, ConfirmModalGeometry, ConfirmModalParams,
};
pub(crate) use crate::components::modal::{render_modal_frame, ModalFrame, ModalTone};
use crate::editor::InputEditor;

pub(crate) mod batch_cancel;
pub(crate) mod confirm;
pub(crate) mod diff_inspector;
pub(crate) mod disconnected;
pub(crate) mod execute_review;
pub(crate) mod execute_scope;
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
    execute_review_params, plan_unfinished_steps, remove_context_file_params,
    render_confirm_cancel_popup, render_confirm_clear_auto_popup,
    render_confirm_clear_history_popup, render_confirm_clear_horizon_final_popup,
    render_confirm_clear_horizon_popup, render_confirm_clear_user_popup,
    render_confirm_delete_plan_final_popup, render_confirm_delete_plan_popup,
    render_confirm_delete_review_popup, render_confirm_detach_focus_popup,
    render_confirm_execute_pipeline_popup, render_confirm_execute_review_popup,
    render_confirm_execute_scope_popup, render_confirm_quit_popup,
    render_confirm_remove_context_file_popup, render_confirm_review_analyze_popup,
    render_confirm_review_truncation_popup, review_analyze_params, review_truncation_params,
};
pub(crate) use diff_inspector::render_diff_inspector;
pub(crate) use disconnected::{disconnected_button_geometry, render_disconnected_popup};
pub(crate) use execute_review::execute_review_dialog_geometry;
pub(crate) use execute_scope::execute_scope_dialog_geometry;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// footer]` without an optional selectable section.
///
/// It delegates to [`picker_dialog_chunks_with_presets`] with zero preset rows,
/// so a dialog whose body holds only the header, the model list, the target
/// field, and the instructions editor keeps exactly the geometry it had before
/// the section existed (Conventions §1.3, §6.7).
pub(super) fn picker_dialog_chunks(
    area: Rect,
    entry_count: usize,
    header_height: u16,
    path_height: u16,
) -> PickerDialogChunks {
    picker_dialog_chunks_with_presets(area, entry_count, header_height, path_height, 0).0
}

/// Splits a picker dialog card into `[header, list, presets, path, editor,
/// buttons, footer]` together with the optional review-preset section that sits
/// between the model list and the target field.
///
/// The whole card chrome — the one-row top inset under the border title, the
/// one-column margin on each side, the pre-action spacer, and the docked hint
/// strip — comes from the shared [`modal_geometry`] formula, so a picker breathes
/// exactly like every other modal. Inside that air-aware body the sections are
/// stacked in visual order and separated by one blank row each, so the model
/// list, the optional preset section, the target field, and the instructions
/// editor read as distinct zones instead of one continuous block
/// (Conventions §6.7, §6.8).
///
/// `preset_rows` is the number of rows the caller offers in the optional
/// section; the section takes one row per requested entry plus its two borders
/// and is truncated by the leftover body, so the target field and the minimal
/// editor always survive. A section requested with zero rows reserves neither a
/// row nor a spacer, which is what keeps the plain picker geometry unchanged.
pub(super) fn picker_dialog_chunks_with_presets(
    area: Rect,
    entry_count: usize,
    header_height: u16,
    path_height: u16,
    preset_rows: usize,
) -> (PickerDialogChunks, Rect) {
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

    // The optional review-preset section keeps its own spacer, the target field,
    // and the minimal editor alive, so it can never starve the controls below
    // it. A section requested with zero rows keeps a zero-height rectangle and
    // reserves no air at all.
    let mut presets = Rect::new(body.x, cursor, body.width, 0);
    if preset_rows > 0 {
        let reserved = PICKER_DIALOG_SPACER_HEIGHT
            .saturating_mul(3)
            .saturating_add(path_height)
            .saturating_add(PICKER_DIALOG_EDITOR_MIN_HEIGHT);
        let section_h = (preset_rows.min(u16::MAX as usize) as u16)
            .saturating_add(2)
            .min(remaining.saturating_sub(reserved));
        if section_h > 0 {
            let gap = PICKER_DIALOG_SPACER_HEIGHT.min(remaining);
            cursor = cursor.saturating_add(gap);
            remaining = remaining.saturating_sub(gap);

            let section_h = section_h.min(remaining);
            presets = Rect::new(body.x, cursor, body.width, section_h);
            cursor = cursor.saturating_add(section_h);
            remaining = remaining.saturating_sub(section_h);
        }
    }

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

    (
        PickerDialogChunks {
            header,
            list,
            path,
            editor,
            buttons: geometry.action_bar,
            footer: geometry.hint_row,
        },
        presets,
    )
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

/// Proportional two-pane geometry of the fullscreen diff inspector (c-3).
///
/// The card claims almost the whole terminal the way the squash dialog does and
/// keeps one row of air above and below. Inside that padded body it splits into
/// a compact file list on the left and an isolated diff read area on the right,
/// while the footer keeps the shared action row and the docked hint strip of
/// every modal, so the audit surface breathes exactly like the rest of the
/// interface (Conventions §6.7, §6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffInspectorGeometry {
    /// Outer card: cleared and filled with the surface role before drawing.
    pub(crate) area: Rect,
    /// Left pane: one row per audited file with its operation badge.
    pub(crate) files: Rect,
    /// Right pane: the isolated diff read area of the highlighted file.
    pub(crate) diff: Rect,
    /// Action row carrying the audit progress.
    pub(crate) action_bar: Rect,
    /// Docked hint strip pinned to the bottom inner row.
    pub(crate) hint_row: Rect,
}

/// Splits the terminal into the inspector card and its two proportional panes.
pub(crate) fn diff_inspector_geometry(term: Rect) -> DiffInspectorGeometry {
    /// Share of the card width the file-list pane claims before the bounds apply.
    /// It must fit a selection marker, an operation badge, a repository path, the
    /// patch count, and the verification badge, so the status of a deep file
    /// never scrolls out of the pane (c-3).
    const FILES_PERCENT: u16 = 45;
    /// Narrowest the file-list pane may become, so a deep repository path stays
    /// readable beside the patch it names.
    const FILES_MIN_WIDTH: u16 = 24;
    /// Widest the file-list pane may grow: reading the patch is the reason the
    /// inspector exists, so the diff pane always keeps the majority of the card.
    const FILES_MAX_WIDTH: u16 = 56;
    /// One column of air between the two panes (Conventions §6.8).
    const PANE_GUTTER: u16 = 1;

    let width = term.width - term.width / 10;
    let height = term.height.saturating_sub(2);
    let area = Rect::new(term.x + (term.width - width) / 2, term.y + 1, width, height);

    // The card chrome itself comes from the shared modal formula, so the audit
    // surface reuses the same air and margins as every dialog (Conventions §6.7).
    let geometry = modal_geometry(area);
    let body = geometry.body;

    let proportional = body.width.saturating_mul(FILES_PERCENT) / 100;
    let bounded = proportional.clamp(FILES_MIN_WIDTH, FILES_MAX_WIDTH);
    // Reading the patch is the point of the audit, so the list never claims half
    // of the card even when its own minimum width would demand it.
    let files_width = bounded.min(body.width / 2);
    // The gutter collapses on a card too narrow to carry it, so the two panes
    // never overlap.
    let gutter = PANE_GUTTER.min(body.width.saturating_sub(files_width));
    let diff_width = body
        .width
        .saturating_sub(files_width)
        .saturating_sub(gutter);

    DiffInspectorGeometry {
        area,
        files: Rect::new(body.x, body.y, files_width, body.height),
        diff: Rect::new(
            body.x.saturating_add(files_width).saturating_add(gutter),
            body.y,
            diff_width,
            body.height,
        ),
        action_bar: geometry.action_bar,
        hint_row: geometry.hint_row,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain picker is exactly the preset-aware layout with no preset rows:
    /// the extended function is the single stacking implementation, so the
    /// refinement and execution dialogs keep the geometry they had
    /// (Conventions §1.3, §6.7).
    #[test]
    fn test_picker_dialog_chunks_delegates_without_presets() {
        let term = Rect::new(0, 0, 140, 60);
        let area = crate::ui::centered_rect(64, 80, term);
        for &(entry_count, header_height, path_height) in &[
            (0usize, 0u16, 0u16),
            (1, 4, 0),
            (3, 6, PICKER_DIALOG_PATH_HEIGHT),
            (9, 6, PICKER_DIALOG_PATH_HEIGHT),
            (20, 12, PICKER_DIALOG_PATH_HEIGHT),
        ] {
            let plain = picker_dialog_chunks(area, entry_count, header_height, path_height);
            let (extended, presets) =
                picker_dialog_chunks_with_presets(area, entry_count, header_height, path_height, 0);

            assert_eq!(plain, extended);
            assert_eq!(presets.height, 0, "no presets, no section");
        }
    }
    /// The audit card claims almost the whole terminal and keeps one row of air
    /// above and below, so the reading surface never touches the screen edges.
    #[test]
    fn test_diff_inspector_card_is_near_fullscreen() {
        let term = Rect::new(0, 0, 120, 40);
        let geometry = diff_inspector_geometry(term);

        assert_eq!(geometry.area.y, term.y + 1);
        assert_eq!(geometry.area.height, term.height - 2);
        assert_eq!(
            geometry.hint_row.y + geometry.hint_row.height,
            geometry.area.y + geometry.area.height - 1,
            "the hint strip is docked above the bottom border"
        );
    }

    /// The padded body holds two panes side by side: the file list stays on the
    /// left, one column of air separates it from the patch, and the diff pane
    /// keeps the majority of the card at every terminal width (c-3).
    #[test]
    fn test_diff_inspector_geometry_splits_the_body_into_two_panes() {
        for term_width in [60u16, 80, 120, 200] {
            let term = Rect::new(0, 0, term_width, 40);
            let geometry = diff_inspector_geometry(term);

            assert_eq!(
                geometry.files.x + geometry.files.width + 1,
                geometry.diff.x,
                "one column of air separates the panes at {term_width} columns"
            );
            assert_eq!(
                geometry.diff.x + geometry.diff.width,
                geometry.area.x + geometry.area.width - 2,
                "the diff pane stops at the right margin at {term_width} columns"
            );
            assert_eq!(geometry.files.y, geometry.diff.y);
            assert_eq!(geometry.files.height, geometry.diff.height);
            assert!(
                geometry.diff.width > geometry.files.width,
                "the patch keeps the majority at {term_width} columns"
            );
        }
    }

    /// A short terminal still yields two side-by-side panes inside the card
    /// instead of an inverted or overflowing split.
    #[test]
    fn test_diff_inspector_geometry_survives_a_short_terminal() {
        let term = Rect::new(0, 0, 62, 12);
        let geometry = diff_inspector_geometry(term);

        assert!(geometry.files.width > 0);
        assert!(geometry.diff.width > 0);
        assert!(geometry.files.height > 0);
        assert_eq!(geometry.files.y, geometry.diff.y);
        assert!(
            geometry.hint_row.y + geometry.hint_row.height < geometry.area.y + geometry.area.height
        );
    }
}
