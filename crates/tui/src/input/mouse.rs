use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tauqe_protocol::{methods, DiscussionFocus};
use tokio::sync::Mutex;

use crate::app::{ActiveModal, AppState, HelpTab, LocaleDialogButton, ViewMode};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

pub const MOUSE_SCROLL_STEP: usize = 3;

pub fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalHit {
    Confirm,
    Cancel,
    Outside,
    InsideBody,
}

pub fn check_confirm_hit(
    geom: &crate::dialogs::render::ConfirmModalGeometry,
    col: u16,
    row: u16,
) -> ModalHit {
    if rect_contains(geom.confirm_button, col, row) {
        ModalHit::Confirm
    } else if rect_contains(geom.cancel_button, col, row) {
        ModalHit::Cancel
    } else if !rect_contains(geom.area, col, row) {
        ModalHit::Outside
    } else {
        ModalHit::InsideBody
    }
}

/// Computes the exact button geometry of a confirmation modal from the same
/// [`ConfirmModalParams`](crate::ui::dialogs::ConfirmModalParams) the renderer
/// uses, so mouse hit-testing can never drift from what is drawn.
fn geometry_of(
    term: Rect,
    params: &crate::dialogs::render::ConfirmModalParams<'_>,
) -> crate::dialogs::render::ConfirmModalGeometry {
    crate::dialogs::render::confirm_modal_geometry(
        term,
        params.width,
        params.body_lines.len(),
        &params.confirm_label,
        &params.cancel_label,
    )
}

pub async fn handle_mouse_event(
    mouse: MouseEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            let term_rect = st.last_frame_area;
            if let Some(ref mut dialog) = st.status_dialog {
                let count = dialog.target.options_count();
                let visible = crate::dialogs::render::status_dialog_options_area(term_rect, count)
                    .height
                    .max(1) as usize;
                dialog.selector.select_prev(visible);
                return Ok(InputResult::Continue);
            }
            if let Some(ref mut dialog) = st.selection_dialog {
                let area =
                    crate::dialogs::render::selection_dialog_area(term_rect, dialog.items.len());
                let visible_height = crate::dialogs::render::selection_dialog_visible_height(area);
                dialog.selector.select_prev(visible_height);
                return Ok(InputResult::Continue);
            }
            // The breakdown outranks Help, so its wheel handling is consulted
            // first and the renderer clamps whatever offset it produces (c-4).
            if let Some(summary) = st.usage_summary.as_mut() {
                summary.scroll = summary.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            // Allow wheel scrolling in squash dialog (diff or file list)
            if let Some(ref mut dialog) = st.squash_dialog {
                if dialog.confirm.is_none() {
                    let (_area, files_rect, _diff_rect) =
                        crate::dialogs::render::squash_dialog_chunks(term_rect);
                    if rect_contains(files_rect, mouse.column, mouse.row) {
                        if dialog.selected_file_index > 0 {
                            dialog.selected_file_index -= 1;
                            dialog.diff_scroll = 0;
                        }
                    } else {
                        dialog.diff_scroll =
                            dialog.diff_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                    }
                    return Ok(InputResult::Continue);
                }
            }
            // Do not scroll background content when modal dialogs are open
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }
            match st.view_mode {
                ViewMode::Repl => {
                    st.model.auto_scroll = false;
                    st.model.scroll = st.model.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::History => {
                    st.history_view.auto_scroll = false;
                    st.history_view.scroll = st
                        .history_view
                        .scroll
                        .saturating_sub(MOUSE_SCROLL_STEP as u16);
                    let h = st.last_model_height;
                    let w = st.history_view.content_width;
                    st.history_view.update_selected_item_from_scroll(h, Some(w));
                }
                ViewMode::Context => {
                    st.context_view.cursor_index = st
                        .context_view
                        .cursor_index
                        .saturating_sub(MOUSE_SCROLL_STEP);
                }
                ViewMode::Review => {
                    st.review.scroll = st.review.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::Plans => {
                    st.plans_view.scroll = st
                        .plans_view
                        .scroll
                        .saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::Onboarding => {}
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::ScrollDown => {
            let term_rect = st.last_frame_area;
            if let Some(ref mut dialog) = st.status_dialog {
                let count = dialog.target.options_count();
                let visible = crate::dialogs::render::status_dialog_options_area(term_rect, count)
                    .height
                    .max(1) as usize;
                dialog.selector.select_next(visible);
                return Ok(InputResult::Continue);
            }
            if let Some(ref mut dialog) = st.selection_dialog {
                let area =
                    crate::dialogs::render::selection_dialog_area(term_rect, dialog.items.len());
                let visible_height = crate::dialogs::render::selection_dialog_visible_height(area);
                dialog.selector.select_next(visible_height);
                return Ok(InputResult::Continue);
            }
            if let Some(summary) = st.usage_summary.as_mut() {
                summary.scroll = summary.scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            // Allow wheel scrolling in squash dialog (diff or file list)
            if let Some(ref mut dialog) = st.squash_dialog {
                if dialog.confirm.is_none() {
                    let (_area, files_rect, _diff_rect) =
                        crate::dialogs::render::squash_dialog_chunks(term_rect);
                    if rect_contains(files_rect, mouse.column, mouse.row) {
                        if !dialog.files.is_empty()
                            && dialog.selected_file_index + 1 < dialog.files.len()
                        {
                            dialog.selected_file_index += 1;
                            dialog.diff_scroll = 0;
                        }
                    } else {
                        dialog.diff_scroll =
                            dialog.diff_scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                    }
                    return Ok(InputResult::Continue);
                }
            }
            // Do not scroll background content when modal dialogs are open
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }
            match st.view_mode {
                ViewMode::Repl => {
                    let view_height = st.last_model_height;
                    let max = st.model.max_scroll(view_height);
                    st.model.scroll =
                        (st.model.scroll.saturating_add(MOUSE_SCROLL_STEP as u16)).min(max);
                    if st.model.scroll >= max {
                        st.model.auto_scroll = true;
                    }
                }
                ViewMode::History => {
                    let view_height = st.last_model_height;
                    let total = st.history_view.rendered_lines_count as u16;
                    let max = total.saturating_sub(view_height);
                    st.history_view.scroll = (st
                        .history_view
                        .scroll
                        .saturating_add(MOUSE_SCROLL_STEP as u16))
                    .min(max);
                    let w = st.history_view.content_width;
                    st.history_view
                        .update_selected_item_from_scroll(view_height, Some(w));
                    if st.history_view.scroll >= max {
                        st.history_view.auto_scroll = true;
                    }
                }
                ViewMode::Context => {
                    let rows_len = st.context_view.compute_rows(&st.context.items).len();
                    if rows_len > 0 {
                        st.context_view.cursor_index =
                            (st.context_view.cursor_index + MOUSE_SCROLL_STEP).min(rows_len - 1);
                    }
                }
                ViewMode::Review => {
                    let max =
                        (st.review.rendered_lines as u16).saturating_sub(st.review.view_height);
                    st.review.scroll = st
                        .review
                        .scroll
                        .saturating_add(MOUSE_SCROLL_STEP as u16)
                        .min(max);
                }
                ViewMode::Plans => {
                    let max = (st.plans_view.rendered_lines as u16)
                        .saturating_sub(st.plans_view.view_height);
                    st.plans_view.scroll = st
                        .plans_view
                        .scroll
                        .saturating_add(MOUSE_SCROLL_STEP as u16)
                        .min(max);
                }
                ViewMode::Onboarding => {}
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let term_rect = st.last_frame_area;
            // Route the click to the canonical top modal so the surface that renders
            // is always the surface that receives the click (single source of truth).
            match st.active_modal() {
                Some(ActiveModal::ServerDisconnected) => {
                    // Single source of truth: the renderer derives the quit
                    // button from the same geometry, so clicks and the drawn
                    // button can never drift (Conventions §6, §7).
                    let geom = crate::dialogs::render::disconnected_button_geometry(term_rect);
                    if rect_contains(geom.quit_button, mouse.column, mouse.row)
                        || !rect_contains(geom.area, mouse.column, mouse.row)
                    {
                        return Ok(InputResult::Exit);
                    }
                }
                Some(ActiveModal::Squash) => {
                    if let Some(ref mut dialog) = st.squash_dialog {
                        if let Some(kind) = dialog.confirm {
                            // Reuse the exact confirmation parameters the
                            // renderer builds, so mouse hit-testing can never
                            // drift from the drawn buttons (Conventions §6, §7).
                            let geom = match kind {
                                crate::app::SquashConfirm::Apply => geometry_of(
                                    term_rect,
                                    &crate::dialogs::render::squash::squash_apply_params(dialog),
                                ),
                                crate::app::SquashConfirm::Discard => geometry_of(
                                    term_rect,
                                    &crate::dialogs::render::squash::squash_discard_params(dialog),
                                ),
                            };
                            match check_confirm_hit(&geom, mouse.column, mouse.row) {
                                ModalHit::Confirm => {
                                    dialog.confirm = None;
                                    match kind {
                                        crate::app::SquashConfirm::Apply => {
                                            let base_ref = dialog.base_ref.clone();
                                            let message =
                                                dialog.message_editor.get_text().trim().to_string();
                                            dialog.applying = true;
                                            dialog.status_message = Some(crate::i18n::active_t(
                                                "dialog.squash.status_applying",
                                            ));
                                            drop(st);
                                            let params = tauqe_protocol::GitSquashApplyParams {
                                                base_ref,
                                                message,
                                            };
                                            send_request(
                                                server_writer,
                                                methods::GIT_SQUASH_APPLY,
                                                serde_json::to_value(params)?,
                                            )
                                            .await?;
                                        }
                                        crate::app::SquashConfirm::Discard => {
                                            st.squash_dialog = None;
                                        }
                                    }
                                    return Ok(InputResult::Continue);
                                }
                                ModalHit::Cancel | ModalHit::Outside => {
                                    dialog.confirm = None;
                                    return Ok(InputResult::Continue);
                                }
                                ModalHit::InsideBody => return Ok(InputResult::Continue),
                            }
                        }

                        let (area, files_rect, _diff_rect) =
                            crate::dialogs::render::squash_dialog_chunks(term_rect);
                        if !rect_contains(area, mouse.column, mouse.row) {
                            if dialog.message_editor.get_text().trim().is_empty() {
                                st.squash_dialog = None;
                            } else {
                                dialog.confirm = Some(crate::app::SquashConfirm::Discard);
                                dialog.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            return Ok(InputResult::Continue);
                        }

                        if rect_contains(files_rect, mouse.column, mouse.row) {
                            let files_visible = files_rect.height.saturating_sub(2) as usize;
                            let files_offset =
                                (dialog.selected_file_index + 1).saturating_sub(files_visible);
                            let clicked_row = (mouse.row.saturating_sub(files_rect.y + 1)) as usize;
                            let clicked_idx = files_offset + clicked_row;
                            if clicked_idx < dialog.files.len() {
                                if clicked_idx == dialog.selected_file_index {
                                    dialog.files[clicked_idx].expanded =
                                        !dialog.files[clicked_idx].expanded;
                                } else {
                                    dialog.selected_file_index = clicked_idx;
                                    dialog.diff_scroll = 0;
                                }
                                dialog.focus = crate::app::SquashDialogFocus::FileList;
                            }
                        }
                    }
                }
                Some(ActiveModal::Review) => {
                    if let Some(mut dialog) = st.review_dialog.take() {
                        let geo = crate::dialogs::render::review_dialog_geometry(
                            term_rect,
                            dialog.selector.len(),
                        );

                        if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                            // Shared with the keyboard launch so a typed target
                            // path rides along on a mouse click too.
                            let params = serde_json::to_value(dialog.start_params())?;
                            crate::dialogs::input::confirm_start_review(
                                state,
                                st,
                                server_writer,
                                params,
                            )
                            .await?;
                            return Ok(InputResult::Continue);
                        }

                        if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                            || !rect_contains(geo.area, mouse.column, mouse.row)
                        {
                            // Safe cancellation: dismiss without restoring.
                            return Ok(InputResult::Continue);
                        }

                        // Clicking a reviewer row selects it and moves focus to
                        // the list, mirroring keyboard navigation.
                        if rect_contains(geo.model_rows, mouse.column, mouse.row) {
                            let visible = geo.model_rows.height as usize;
                            let start = dialog.selector.visible_start(visible);
                            let clicked_row = (mouse.row - geo.model_rows.y) as usize;
                            let index = start + clicked_row;
                            if index < dialog.selector.len() {
                                dialog.selector.select_index(index);
                            }
                            dialog.focus = crate::app::ReviewDialogFocus::ModelList;
                        } else if rect_contains(geo.path_area, mouse.column, mouse.row) {
                            // Clicking the target-paths field focuses it so the
                            // next keystroke edits the clicked input.
                            dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
                        } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                            dialog.focus = crate::app::ReviewDialogFocus::PromptEditor;
                        }

                        st.review_dialog = Some(dialog);
                    }
                }
                Some(ActiveModal::ReviewRefine) => {
                    if let Some(mut dialog) = st.review.refine_dialog.take() {
                        let geo = crate::dialogs::render::refine::refine_dialog_geometry(
                            term_rect,
                            dialog.selector.len(),
                        );

                        if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                            crate::dialogs::input::submit_review_refine_dialog(
                                state,
                                st,
                                dialog,
                                server_writer,
                            )
                            .await?;
                            return Ok(InputResult::Continue);
                        }

                        if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                            || !rect_contains(geo.area, mouse.column, mouse.row)
                        {
                            // Safe cancellation: dismiss without starting a turn.
                            return Ok(InputResult::Continue);
                        }

                        // Clicking an architect row selects it and moves focus to
                        // the list, mirroring keyboard navigation.
                        if rect_contains(geo.model_rows, mouse.column, mouse.row) {
                            let visible = geo.model_rows.height as usize;
                            let start = dialog.selector.visible_start(visible);
                            let clicked_row = (mouse.row - geo.model_rows.y) as usize;
                            let index = start + clicked_row;
                            if index < dialog.selector.len() {
                                dialog.selector.select_index(index);
                            }
                            dialog.focus = crate::views::plans::RefineDialogFocus::ModelList;
                        } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                            dialog.focus =
                                crate::views::plans::RefineDialogFocus::InstructionsEditor;
                        }

                        st.review.refine_dialog = Some(dialog);
                    }
                }
                Some(ActiveModal::ConfirmCancel) => {
                    if st.confirm_cancel {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::confirm_cancel_params(&st),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_cancel = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                let cancel_review = st.review.running;
                                let cancel_model = st.model.is_busy();
                                drop(st);
                                if cancel_review {
                                    send_request(
                                        server_writer,
                                        methods::REVIEW_CANCEL,
                                        serde_json::json!({}),
                                    )
                                    .await?;
                                }
                                if cancel_model || !cancel_review {
                                    send_request(
                                        server_writer,
                                        methods::MODEL_CANCEL,
                                        serde_json::json!({}),
                                    )
                                    .await?;
                                }
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_cancel = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ConfirmQuit) => {
                    if st.confirm_quit {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::confirm_quit_params(&st),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_quit = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                return Ok(InputResult::Exit);
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_quit = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::BatchCancel) => {
                    let geom = crate::dialogs::render::batch_cancel_modal_geometry(term_rect);
                    let choice = if rect_contains(geom.continue_button, mouse.column, mouse.row) {
                        Some(crate::app::BatchCancelButton::Continue)
                    } else if rect_contains(geom.soft_stop_button, mouse.column, mouse.row) {
                        Some(crate::app::BatchCancelButton::SoftStop)
                    } else if rect_contains(geom.hard_stop_button, mouse.column, mouse.row) {
                        Some(crate::app::BatchCancelButton::HardStop)
                    } else {
                        None
                    };

                    match choice {
                        Some(choice) => {
                            crate::dialogs::input::apply_batch_cancel(
                                &mut st,
                                choice,
                                server_writer,
                            )
                            .await?;
                        }
                        None => {
                            st.confirm_batch_cancel = false;
                            st.batch_cancel_button = crate::app::BatchCancelButton::Continue;
                        }
                    }
                    return Ok(InputResult::Continue);
                }
                Some(ActiveModal::ClearAuto) => {
                    if st.context_view.confirm_clear_auto {
                        let geom =
                            geometry_of(term_rect, &crate::dialogs::render::clear_auto_params(&st));
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.context_view.confirm_clear_auto = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                drop(st);
                                let params = tauqe_protocol::ContextClearParams {
                                    layer: Some(tauqe_protocol::ContextLayer::Auto),
                                };
                                send_request(
                                    server_writer,
                                    methods::CONTEXT_CLEAR,
                                    serde_json::to_value(params)?,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.context_view.confirm_clear_auto = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ClearUser) => {
                    if st.context_view.confirm_clear_user {
                        let geom =
                            geometry_of(term_rect, &crate::dialogs::render::clear_user_params(&st));
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.context_view.confirm_clear_user = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                drop(st);
                                let params = tauqe_protocol::ContextClearParams {
                                    layer: Some(tauqe_protocol::ContextLayer::User),
                                };
                                send_request(
                                    server_writer,
                                    methods::CONTEXT_CLEAR,
                                    serde_json::to_value(params)?,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.context_view.confirm_clear_user = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ConfirmDetachFocus) => {
                    if st.confirm_detach_focus {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::confirm_detach_focus_params(&st),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_detach_focus = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                st.discussion_focus = DiscussionFocus::None;
                                drop(st);
                                send_request(
                                    server_writer,
                                    methods::DISCUSSION_CLEAR_FOCUS,
                                    serde_json::json!({}),
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_detach_focus = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ClearHorizon) => {
                    if let Some(plan_id) = st.confirm_clear_horizon.clone() {
                        // Reuse the exact confirmation parameters the renderer
                        // draws, so hit-testing never desynchronizes from the
                        // rendered buttons (Conventions §6, §7).
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::clear_horizon_params(&st, &plan_id),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_clear_horizon = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                // A horizon that already carries started or
                                // completed steps has committed work in Git: the
                                // reset escalates to the second tier instead of
                                // dropping recorded history silently.
                                let recorded = st
                                    .plans_view
                                    .plans
                                    .iter()
                                    .find(|p| p.id == plan_id)
                                    .is_some_and(|p| p.has_started_or_completed_steps());
                                if recorded {
                                    st.confirm_clear_horizon_final = Some(plan_id);
                                } else {
                                    // The success notice is emitted once the
                                    // server confirms the reset in the response
                                    // handler, not on the click path.
                                    drop(st);
                                    crate::dialogs::input::clear_plan_horizon(
                                        server_writer,
                                        plan_id,
                                    )
                                    .await?;
                                }
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_clear_horizon = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ClearHorizonFinal) => {
                    if let Some(plan_id) = st.confirm_clear_horizon_final.clone() {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::clear_horizon_final_params(&st, &plan_id),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_clear_horizon_final = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                drop(st);
                                crate::dialogs::input::clear_plan_horizon(server_writer, plan_id)
                                    .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_clear_horizon_final = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::DeletePlan) => {
                    if let Some(plan_id) = st.confirm_delete_plan.clone() {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::delete_plan_params(&st, &plan_id),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_delete_plan = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                drop(st);
                                crate::dialogs::input::delete_plan(server_writer, plan_id).await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_delete_plan = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::DeleteReview) => {
                    if let Some(review_id) = st.confirm_delete_review.clone() {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::delete_review_params(&st, &review_id),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_delete_review = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                drop(st);
                                crate::dialogs::input::delete_review(server_writer, review_id)
                                    .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_delete_review = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ExecuteReview) => {
                    // Point execution draws the Coder form dialog, whose action
                    // buttons, model rows, and editor area come from one geometry
                    // source shared with the renderer (Conventions §6, §7).
                    let single_target = st
                        .confirm_execute_review
                        .as_ref()
                        .is_some_and(|target| target.is_single_target());
                    if single_target {
                        if let Some(mut target) = st.confirm_execute_review.take() {
                            let geo = crate::dialogs::render::execute_review_dialog_geometry(
                                term_rect,
                                target.selector.len(),
                            );

                            if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::confirm_execute_review(
                                    state,
                                    st,
                                    server_writer,
                                    target,
                                )
                                .await?;
                                return Ok(InputResult::Continue);
                            }

                            if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                                || !rect_contains(geo.area, mouse.column, mouse.row)
                            {
                                // Safe cancellation: dismiss without a turn.
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                return Ok(InputResult::Continue);
                            }

                            if rect_contains(geo.model_rows, mouse.column, mouse.row) {
                                let visible = geo.model_rows.height as usize;
                                let start = target.selector.visible_start(visible);
                                let clicked_row = (mouse.row - geo.model_rows.y) as usize;
                                let index = start + clicked_row;
                                if index < target.selector.len() {
                                    target.selector.select_index(index);
                                }
                                target.focus = crate::views::plans::RefineDialogFocus::ModelList;
                            } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                                target.focus =
                                    crate::views::plans::RefineDialogFocus::InstructionsEditor;
                            }

                            st.confirm_execute_review = Some(target);
                        }
                        return Ok(InputResult::Continue);
                    }
                    if let Some(target) = st.confirm_execute_review.clone() {
                        let geom = match crate::dialogs::render::execute_review_params(&st) {
                            Some(params) => geometry_of(term_rect, &params),
                            None => return Ok(InputResult::Continue),
                        };
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_execute_review = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::confirm_execute_review(
                                    state,
                                    st,
                                    server_writer,
                                    target,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_execute_review = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ExecutePipeline) => {
                    // A pipeline is a super-batch over several plans, so the
                    // confirmation is the binary confirm/cancel card whose
                    // geometry is derived from the same parameters the renderer
                    // draws (Conventions §6, §7).
                    if let Some(target) = st.confirm_execute_pipeline.clone() {
                        let geom = match crate::dialogs::render::execute_pipeline_params(&st) {
                            Some(params) => geometry_of(term_rect, &params),
                            None => return Ok(InputResult::Continue),
                        };
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_execute_pipeline = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::confirm_execute_pipeline(
                                    state,
                                    st,
                                    server_writer,
                                    target,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_execute_pipeline = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::ExecuteScope) => {
                    if let Some(target) = st.confirm_execute_scope.clone() {
                        let geom = match crate::dialogs::render::execute_scope_params(&st) {
                            Some(params) => geometry_of(term_rect, &params),
                            None => return Ok(InputResult::Continue),
                        };
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_execute_scope = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::confirm_execute_scope(
                                    state,
                                    st,
                                    server_writer,
                                    target,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_execute_scope = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::RefineDialog) => {
                    if let Some(mut dialog) = st.plans_view.refine_dialog.take() {
                        let geo = crate::dialogs::render::refine_dialog_geometry(
                            term_rect,
                            dialog.selector.len(),
                        );

                        if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                            crate::dialogs::input::submit_refine_dialog(
                                state,
                                st,
                                dialog,
                                server_writer,
                            )
                            .await?;
                            return Ok(InputResult::Continue);
                        }

                        if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                            || !rect_contains(geo.area, mouse.column, mouse.row)
                        {
                            // Safe cancellation: dismiss without starting a turn.
                            return Ok(InputResult::Continue);
                        }

                        // Clicking an architect row selects it and moves focus to
                        // the list, mirroring keyboard navigation.
                        if rect_contains(geo.model_rows, mouse.column, mouse.row) {
                            let visible = geo.model_rows.height as usize;
                            let start = dialog.selector.visible_start(visible);
                            let clicked_row = (mouse.row - geo.model_rows.y) as usize;
                            let index = start + clicked_row;
                            if index < dialog.selector.len() {
                                dialog.selector.select_index(index);
                            }
                            dialog.focus = crate::views::plans::RefineDialogFocus::ModelList;
                        } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                            // Clicking the instructions editor focuses it so the
                            // next keystroke edits the clicked input.
                            dialog.focus =
                                crate::views::plans::RefineDialogFocus::InstructionsEditor;
                        }

                        st.plans_view.refine_dialog = Some(dialog);
                    }
                }
                Some(ActiveModal::RefineBlocked) => {
                    if let Some(blocked) = st.plans_view.refine_blocked.clone() {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::refine_blocked_params(
                                &blocked,
                                st.confirm_button,
                            ),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::submit_refine_blocked(
                                    state,
                                    st,
                                    blocked,
                                    server_writer,
                                )
                                .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.plans_view.refine_blocked = None;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::Prompt) => {
                    // Confirm submits the comment exactly as `Enter` does;
                    // Cancel and any click outside dismiss the modal safely.
                    let (confirm_btn, cancel_btn) =
                        crate::dialogs::render::prompt_modal_buttons(term_rect);
                    if rect_contains(confirm_btn, mouse.column, mouse.row) {
                        if let Some(dialog) = st.prompt_modal.take() {
                            let _ = crate::dialogs::input::submit_prompt_modal(
                                st,
                                dialog,
                                server_writer,
                            )
                            .await?;
                        }
                    } else if rect_contains(cancel_btn, mouse.column, mouse.row)
                        || !rect_contains(
                            crate::dialogs::render::prompt_modal_area(term_rect),
                            mouse.column,
                            mouse.row,
                        )
                    {
                        st.prompt_modal = None;
                    }
                }
                Some(ActiveModal::ThemeSelector) => {
                    // Clicking a preset previews it; clicking outside cancels,
                    // restoring the preset that was active on open.
                    crate::theme::selector::handle_theme_selector_click(
                        &mut st,
                        mouse.column,
                        mouse.row,
                        term_rect,
                    );
                }
                Some(ActiveModal::ClearHistory) => {
                    if st.confirm_clear_history {
                        let geom = geometry_of(
                            term_rect,
                            &crate::dialogs::render::clear_history_params(&st),
                        );
                        match check_confirm_hit(&geom, mouse.column, mouse.row) {
                            ModalHit::Confirm => {
                                st.confirm_clear_history = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                                crate::dialogs::input::confirm_clear_history(st, server_writer)
                                    .await?;
                            }
                            ModalHit::Cancel | ModalHit::Outside => {
                                st.confirm_clear_history = false;
                                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            }
                            ModalHit::InsideBody => {}
                        }
                    }
                }
                Some(ActiveModal::Status) => {
                    if let Some(dialog) = st.status_dialog.clone() {
                        let options_count = dialog.target.options_count();
                        let area =
                            crate::dialogs::render::status_dialog_area(term_rect, options_count);
                        let options_area = crate::dialogs::render::status_dialog_options_area(
                            term_rect,
                            options_count,
                        );
                        let (apply_btn, cancel_btn) =
                            crate::dialogs::render::status_dialog_buttons(term_rect, options_count);

                        // The action bar carries two distinct buttons: Apply
                        // commits the highlighted status, Cancel dismisses. Hit
                        // each button by its rendered bounds so a click on
                        // Apply can never fall through to the dismiss path.
                        if rect_contains(apply_btn, mouse.column, mouse.row) {
                            let chosen = dialog.selector.selected_index();
                            crate::dialogs::input::apply_status_dialog(
                                &mut st,
                                chosen,
                                server_writer,
                            )
                            .await?;
                        } else if rect_contains(cancel_btn, mouse.column, mouse.row) {
                            st.status_dialog = None;
                        } else if rect_contains(options_area, mouse.column, mouse.row) {
                            // Map the clicked visual row onto the list index,
                            // honoring the current scroll offset and the exact
                            // window the rows are drawn in, so a click on empty
                            // space below the last option never selects a status
                            // (Conventions §6.6).
                            let clicked_row = dialog.selector.scroll_offset()
                                + (mouse.row - options_area.y) as usize;
                            if clicked_row < options_count {
                                crate::dialogs::input::apply_status_dialog(
                                    &mut st,
                                    clicked_row,
                                    server_writer,
                                )
                                .await?;
                            }
                        } else if !rect_contains(area, mouse.column, mouse.row) {
                            st.status_dialog = None;
                        }
                    }
                }
                Some(ActiveModal::Selection) => {
                    if let Some(dialog) = st.selection_dialog.as_ref() {
                        let item_count = dialog.items.len();
                        let area =
                            crate::dialogs::render::selection_dialog_area(term_rect, item_count);
                        let (select_btn, cancel_btn) =
                            crate::dialogs::render::selection::selection_dialog_buttons(
                                term_rect, item_count,
                            );

                        // Action footer: Select activates the highlighted row, Cancel dismisses.
                        if rect_contains(select_btn, mouse.column, mouse.row) {
                            let selected =
                                dialog.items.get(dialog.selector.selected_index()).cloned();
                            if let Some(item) = selected {
                                crate::dialogs::input::activate_selection_item(
                                    st,
                                    item,
                                    server_writer,
                                )
                                .await?;
                            }
                            return Ok(InputResult::Continue);
                        }
                        if rect_contains(cancel_btn, mouse.column, mouse.row) {
                            st.selection_dialog = None;
                            return Ok(InputResult::Continue);
                        }

                        if !rect_contains(area, mouse.column, mouse.row) {
                            st.selection_dialog = None;
                            return Ok(InputResult::Continue);
                        }

                        // Rows resolve through the same list rectangle the
                        // renderer draws, so the air above the first row and
                        // below the last one never activates an item
                        // (Conventions §6.6, §6.7, c-3).
                        let list_area =
                            crate::dialogs::render::selection::selection_dialog_list_area(
                                term_rect, item_count,
                            );
                        if rect_contains(list_area, mouse.column, mouse.row) {
                            let clicked_visible = (mouse.row - list_area.y) as usize;
                            let clicked_idx = dialog.selector.scroll_offset() + clicked_visible;
                            let clicked = dialog.items.get(clicked_idx).cloned();
                            if let Some(item) = clicked {
                                crate::dialogs::input::activate_selection_item(
                                    st,
                                    item,
                                    server_writer,
                                )
                                .await?;
                            }
                        }
                    }
                }
                Some(ActiveModal::Locale) => {
                    // The three action buttons are clickable through the same
                    // geometry the renderer draws; a click outside the dialog
                    // dismisses it and a click elsewhere inside leaves the
                    // language field to the keyboard (Conventions §6.6).
                    let geom = crate::dialogs::render::locale_dialog_geometry(term_rect);
                    let button = if rect_contains(geom.synthesize_button, mouse.column, mouse.row) {
                        Some(LocaleDialogButton::Synthesize)
                    } else if rect_contains(geom.reset_button, mouse.column, mouse.row) {
                        Some(LocaleDialogButton::ResetEnglish)
                    } else if rect_contains(geom.cancel_button, mouse.column, mouse.row) {
                        Some(LocaleDialogButton::Cancel)
                    } else {
                        None
                    };

                    if let Some(button) = button {
                        crate::dialogs::input::click_locale_button(st, button, server_writer)
                            .await?;
                        return Ok(InputResult::Continue);
                    }
                    if !rect_contains(geom.area, mouse.column, mouse.row) {
                        st.locale_dialog = None;
                    }
                }
                Some(ActiveModal::UsageSummary) => {
                    // The breakdown exposes no controls: a click outside its
                    // frame dismisses it and a click inside leaves it open for
                    // the wheel and the keyboard (Conventions §6).
                    let area = crate::dialogs::render::usage_summary_area(term_rect);
                    if !rect_contains(area, mouse.column, mouse.row) {
                        st.usage_summary = None;
                    }
                }
                Some(ActiveModal::Help) => {
                    // A click on a header tab switches that tab; any other
                    // click dismisses the dialog, preserving the click-to-close
                    // affordance while making the tabs reachable
                    // (Conventions §6.6).
                    let geom = crate::dialogs::render::help::help_tab_geometry(term_rect);
                    if rect_contains(geom.global_tab, mouse.column, mouse.row) {
                        st.help_tab = HelpTab::Global;
                        st.help_scroll = 0;
                    } else if rect_contains(geom.current_mode_tab, mouse.column, mouse.row) {
                        st.help_tab = HelpTab::CurrentMode;
                        st.help_scroll = 0;
                    } else if rect_contains(geom.text_editor_tab, mouse.column, mouse.row) {
                        st.help_tab = HelpTab::TextEditor;
                        st.help_scroll = 0;
                    } else {
                        st.show_help = false;
                        st.help_scroll = 0;
                    }
                }
                None => {
                    // No modal: protect against inline Context input, then route to
                    // the header and viewport hit targets.
                    if st.has_active_modal() {
                        return Ok(InputResult::Continue);
                    }

                    if st.header_clicks.row > 0 && mouse.row == st.header_clicks.row {
                        let areas = st.header_clicks;
                        if areas.repl_tab.1 > 0
                            && mouse.column >= areas.repl_tab.0
                            && mouse.column <= areas.repl_tab.1
                        {
                            crate::input::switch_view(&mut st, ViewMode::Repl, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        } else if areas.plans_tab.1 > 0
                            && mouse.column >= areas.plans_tab.0
                            && mouse.column <= areas.plans_tab.1
                        {
                            crate::input::switch_view(&mut st, ViewMode::Plans, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        } else if areas.review_tab.1 > 0
                            && mouse.column >= areas.review_tab.0
                            && mouse.column <= areas.review_tab.1
                        {
                            crate::input::switch_view(&mut st, ViewMode::Review, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        } else if areas.context_tab.1 > 0
                            && mouse.column >= areas.context_tab.0
                            && mouse.column <= areas.context_tab.1
                        {
                            crate::input::switch_view(&mut st, ViewMode::Context, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        } else if areas.history_tab.1 > 0
                            && mouse.column >= areas.history_tab.0
                            && mouse.column <= areas.history_tab.1
                        {
                            crate::input::switch_view(&mut st, ViewMode::History, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        } else if areas.model_select.1 > 0
                            && mouse.column >= areas.model_select.0
                            && mouse.column <= areas.model_select.1
                        {
                            if st.model.is_busy() {
                                st.notify_warning(crate::i18n::active_t(
                                    "notify.model_select_busy",
                                ));
                                return Ok(InputResult::Continue);
                            }
                            if st.open_model_dialog() {
                                return Ok(InputResult::Continue);
                            }
                        } else if areas.squash_button.1 > 0
                            && mouse.column >= areas.squash_button.0
                            && mouse.column <= areas.squash_button.1
                        {
                            crate::input::open_squash_dialog(&mut st, server_writer).await?;
                            return Ok(InputResult::Continue);
                        } else if areas.help_button.1 > 0
                            && mouse.column >= areas.help_button.0
                            && mouse.column <= areas.help_button.1
                        {
                            st.show_help = !st.show_help;
                            return Ok(InputResult::Continue);
                        }
                    }

                    if st.view_mode == ViewMode::Repl {
                        let (cx, cy, cw, ch) = st.model.content_rect;
                        if mouse.column >= cx
                            && mouse.column < cx + cw
                            && mouse.row >= cy
                            && mouse.row < cy + ch
                        {
                            let relative_row = (mouse.row - cy) as usize;
                            let clicked_visual_line = st.model.scroll as usize + relative_row;

                            if let Some(block) = st
                                .model
                                .code_blocks
                                .iter()
                                .find(|b| {
                                    clicked_visual_line >= b.visual_start_line
                                        && clicked_visual_line <= b.visual_end_line
                                })
                                .cloned()
                            {
                                let (btn_rel_start, btn_rel_end) =
                                    crate::markdown::copy_button_span(&block.lang);
                                let btn_start = cx + btn_rel_start;
                                let btn_end = cx + btn_rel_end;
                                let is_button_clicked = clicked_visual_line
                                    == block.visual_start_line
                                    && mouse.column >= btn_start
                                    && mouse.column <= btn_end;

                                if is_button_clicked {
                                    let code_to_copy = block.code.clone();
                                    let block_id = block.id;
                                    match crate::clipboard::copy_to_clipboard(&code_to_copy) {
                                        crate::clipboard::CopyResult::Native => {
                                            st.model.copy_flash =
                                                Some((block_id, std::time::Instant::now()));
                                            let lines_count = code_to_copy.lines().count().max(1);
                                            st.notify_success(
                                                crate::i18n::active_t("notify.code_copied")
                                                    .replace("{}", &lines_count.to_string()),
                                            );
                                            st.model.update_markdown();
                                        }
                                        crate::clipboard::CopyResult::Osc52Only => {
                                            st.model.copy_flash =
                                                Some((block_id, std::time::Instant::now()));
                                            let lines_count = code_to_copy.lines().count().max(1);
                                            st.notify_success(
                                                crate::i18n::active_t("notify.code_copied_osc52")
                                                    .replace("{}", &lines_count.to_string()),
                                            );
                                            st.model.update_markdown();
                                        }
                                        crate::clipboard::CopyResult::Failed => {
                                            st.notify_error(crate::i18n::active_t(
                                                "notify.code_copy_failed",
                                            ));
                                        }
                                    }
                                } else if !st.shift_tip_shown {
                                    st.notify_info(crate::i18n::active_t("notify.mouse_shift_tip"));
                                    st.shift_tip_shown = true;
                                }
                            }
                        }
                    } else if st.view_mode == ViewMode::History {
                        let (cx, cy, cw, ch) = st.history_view.content_rect;
                        if mouse.column >= cx
                            && mouse.column < cx + cw
                            && mouse.row >= cy
                            && mouse.row < cy + ch
                        {
                            let relative_row = (mouse.row - cy) as usize;
                            let clicked_visual_line =
                                st.history_view.scroll as usize + relative_row;

                            if let Some(block) = st
                                .history_view
                                .code_blocks
                                .iter()
                                .find(|b| {
                                    clicked_visual_line >= b.visual_start_line
                                        && clicked_visual_line <= b.visual_end_line
                                })
                                .cloned()
                            {
                                let (btn_rel_start, btn_rel_end) =
                                    crate::markdown::copy_button_span(&block.lang);
                                // History bodies render with a two-column line prefix.
                                let btn_start = cx + 2 + btn_rel_start;
                                let btn_end = cx + 2 + btn_rel_end;
                                let is_button_clicked = clicked_visual_line
                                    == block.visual_start_line
                                    && mouse.column >= btn_start
                                    && mouse.column <= btn_end;

                                if is_button_clicked {
                                    let code_to_copy = block.code.clone();
                                    match crate::clipboard::copy_to_clipboard(&code_to_copy) {
                                        crate::clipboard::CopyResult::Native => {
                                            st.history_view.copy_flash = Some((
                                                (block.item_id, block.block_id),
                                                std::time::Instant::now(),
                                            ));
                                            let lines_count = code_to_copy.lines().count().max(1);
                                            st.notify_success(
                                                crate::i18n::active_t("notify.code_copied")
                                                    .replace("{}", &lines_count.to_string()),
                                            );
                                        }
                                        crate::clipboard::CopyResult::Osc52Only => {
                                            st.history_view.copy_flash = Some((
                                                (block.item_id, block.block_id),
                                                std::time::Instant::now(),
                                            ));
                                            let lines_count = code_to_copy.lines().count().max(1);
                                            st.notify_success(
                                                crate::i18n::active_t("notify.code_copied_osc52")
                                                    .replace("{}", &lines_count.to_string()),
                                            );
                                        }
                                        crate::clipboard::CopyResult::Failed => {
                                            st.notify_error(crate::i18n::active_t(
                                                "notify.code_copy_failed",
                                            ));
                                        }
                                    }
                                } else if !st.shift_tip_shown {
                                    st.notify_info(crate::i18n::active_t("notify.mouse_shift_tip"));
                                    st.shift_tip_shown = true;
                                }
                            }
                        }
                    }
                }
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::Down(MouseButton::Middle) => {
            // Middle-click pastes the terminal Primary Selection. Routing the
            // text through `handle_paste` keeps the modal stack authoritative:
            // only the visible, top-most surface ever receives the paste.
            if let Some(text) = crate::clipboard::get_primary_or_clipboard() {
                crate::input::handle_paste(&mut st, &text);
            }
            Ok(InputResult::Continue)
        }
        _ => Ok(InputResult::Continue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    #[test]
    fn test_rect_contains_and_check_confirm_hit() {
        let term = Rect::new(0, 0, 100, 30);
        let geom = crate::dialogs::render::confirm_modal_geometry(
            term,
            58,
            5,
            "Interrupt (Y)",
            "Cancel (Esc)",
        );

        assert!(geom.area.width > 0 && geom.area.height > 0);
        assert!(geom.confirm_button.width > 0 && geom.cancel_button.width > 0);

        // Click exactly on Confirm button
        let hit = check_confirm_hit(&geom, geom.confirm_button.x, geom.confirm_button.y);
        assert_eq!(hit, ModalHit::Confirm);

        // Click exactly on Cancel button
        let hit = check_confirm_hit(&geom, geom.cancel_button.x, geom.cancel_button.y);
        assert_eq!(hit, ModalHit::Cancel);

        // Click outside modal area
        let hit = check_confirm_hit(&geom, 0, 0);
        assert_eq!(hit, ModalHit::Outside);

        // Click inside modal body (above button row)
        let hit = check_confirm_hit(&geom, geom.area.x + 5, geom.area.y + 2);
        assert_eq!(hit, ModalHit::InsideBody);
    }

    #[tokio::test]
    async fn test_middle_click_routes_primary_selection_paste() {
        let state = Arc::new(Mutex::new(crate::app::test_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Middle),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        // The handler must never panic regardless of the clipboard contents;
        // in a headless environment the paste source degrades to `None`.
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);
    }

    /// A click on the status dialog's `[ Apply ]` button must commit the
    /// highlighted status instead of dismissing the dialog without applying.
    #[tokio::test]
    async fn status_apply_button_click_applies_selected_status() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Plans;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-1".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        }];
        st.status_dialog = Some(crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: tauqe_protocol::PlanItemStatus::Todo,
            },
            3, // DONE
        ));

        let term = st.last_frame_area;
        let (apply_btn, _cancel_btn) = crate::dialogs::render::status_dialog_buttons(term, 6);
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: apply_btn.x + 2,
            row: apply_btn.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        assert!(st.status_dialog.is_none(), "the dialog must be consumed");
        let item = st
            .plans_view
            .plans
            .iter()
            .find(|p| p.id == "plan-1")
            .and_then(|p| p.find_item("1"))
            .expect("plan item must exist");
        assert_eq!(
            item.status,
            tauqe_protocol::PlanItemStatus::Done,
            "Apply must commit the highlighted status"
        );
    }

    /// A click on the point-execution `Resolve` button must launch the same
    /// bounded Coder turn the keyboard confirmation starts.
    #[tokio::test]
    async fn execute_review_confirm_click_launches_turn() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        let entries = st.coder_entries();
        st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "All remaining TODO findings".to_string(),
            Some(1),
            None,
            vec![tauqe_protocol::ReviewIssue {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                issue_type: tauqe_protocol::IssueType::Architecture,
                status: tauqe_protocol::IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }],
            entries,
        ));

        let term = st.last_frame_area;
        let entry_count = st
            .confirm_execute_review
            .as_ref()
            .map(|target| target.selector.len())
            .unwrap_or(0);
        let geom = crate::dialogs::render::execute_review_dialog_geometry(term, entry_count);

        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geom.confirm_button.x + 1,
            row: geom.confirm_button.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        assert_eq!(st.view_mode, ViewMode::Repl);
        assert!(st.confirm_execute_review.is_none());
    }

    /// A click on a Help header tab must switch to that tab without closing the
    /// dialog.
    #[tokio::test]
    async fn help_tab_click_switches_tab_without_closing() {
        let mut st = crate::app::test_state();
        st.show_help = true;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let geo = crate::dialogs::render::help::help_tab_geometry(Rect::new(0, 0, 100, 30));
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geo.current_mode_tab.x + 1,
            row: geo.current_mode_tab.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        assert!(st.show_help, "clicking a tab must not close the dialog");
        assert_eq!(st.help_tab, HelpTab::CurrentMode);
    }

    /// A click on the locale dialog's `Cancel` button must dismiss it.
    #[tokio::test]
    async fn locale_cancel_button_click_dismisses_dialog() {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.locale_dialog = Some(crate::app::LocaleDialogState::default());
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let geo = crate::dialogs::render::locale_dialog_geometry(Rect::new(0, 0, 100, 30));
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geo.cancel_button.x + 1,
            row: geo.cancel_button.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        assert!(st.locale_dialog.is_none());
    }

    /// The usage breakdown dismisses on a click outside its frame and stays
    /// open for a click inside (Conventions §6).
    #[tokio::test]
    async fn usage_summary_click_outside_closes_and_inside_keeps_it() {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.open_usage_summary();
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let area = crate::dialogs::render::usage_summary_area(Rect::new(0, 0, 100, 30));
        handle_mouse_event(left_click(area.x + 2, area.y + 2), &state, &mut conn)
            .await
            .unwrap();
        assert!(state.lock().await.usage_summary.is_some());

        handle_mouse_event(left_click(0, 0), &state, &mut conn)
            .await
            .unwrap();
        assert!(state.lock().await.usage_summary.is_none());
    }

    /// The wheel scrolls the breakdown one step per notch, like every other
    /// scrollable surface.
    #[tokio::test]
    async fn usage_summary_wheel_scrolls_the_breakdown() {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.open_usage_summary();
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 15,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(down, &state, &mut conn).await.unwrap();
        assert_eq!(
            state.lock().await.usage_summary.as_ref().unwrap().scroll,
            MOUSE_SCROLL_STEP as u16
        );

        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..down
        };
        handle_mouse_event(up, &state, &mut conn).await.unwrap();
        assert_eq!(state.lock().await.usage_summary.as_ref().unwrap().scroll, 0);
    }

    fn architect_entry(name: &str) -> tauqe_protocol::ModelEntry {
        tauqe_protocol::ModelEntry::new(tauqe_protocol::ModelRef::openrouter(name), None, None)
    }

    fn state_with_review_refine_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 120, 40);
        st.review.refine_dialog = Some(crate::views::review::ReviewRefineDialogState::new(
            "rev-1".to_string(),
            "Session rev-1".to_string(),
            2,
            "Missing auth guard".to_string(),
            vec![architect_entry("arch-a"), architect_entry("arch-b")],
        ));
        st
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// A click on the review-refine `Start Refinement` button must launch the
    /// same `review/refineItem` request the keyboard confirm does.
    #[tokio::test]
    async fn review_refine_confirm_click_submits_the_request() {
        let st = state_with_review_refine_dialog();
        let geo = crate::dialogs::render::refine::refine_dialog_geometry(st.last_frame_area, 2);
        let mouse = left_click(geo.confirm_button.x + 1, geo.confirm_button.y);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/refineItem"), "{payload}");

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_none());
        assert_eq!(guard.review.refining.as_deref(), Some("rev-1"));
    }

    /// A click on the `Cancel` button must dismiss the dialog without sending a
    /// request, leaving the Review view idle.
    #[tokio::test]
    async fn review_refine_cancel_click_dismisses_without_a_request() {
        let st = state_with_review_refine_dialog();
        let geo = crate::dialogs::render::refine::refine_dialog_geometry(st.last_frame_area, 2);
        let mouse = left_click(geo.cancel_button.x + 1, geo.cancel_button.y);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let guard = state.lock().await;
        assert!(guard.review.refine_dialog.is_none());
        assert!(guard.review.refining.is_none());
        assert!(conn.written().is_empty());
    }

    /// Clicking an architect row selects that model and moves focus to the
    /// list, mirroring keyboard list navigation.
    #[tokio::test]
    async fn review_refine_model_row_click_selects_and_focuses_the_list() {
        let st = state_with_review_refine_dialog();
        let geo = crate::dialogs::render::refine::refine_dialog_geometry(st.last_frame_area, 2);
        let mouse = left_click(geo.model_rows.x + 1, geo.model_rows.y + 1);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let guard = state.lock().await;
        let dialog = guard
            .review
            .refine_dialog
            .as_ref()
            .expect("dialog stays open");
        assert_eq!(dialog.selector.selected_index(), 1);
        assert_eq!(
            dialog.focus,
            crate::views::plans::RefineDialogFocus::ModelList
        );
        assert!(conn.written().is_empty());
    }

    /// Clicking the instructions area must move focus there so the next
    /// keystroke edits the clicked field.
    #[tokio::test]
    async fn review_refine_editor_click_moves_focus_and_keeps_the_dialog() {
        let st = state_with_review_refine_dialog();
        let geo = crate::dialogs::render::refine::refine_dialog_geometry(st.last_frame_area, 2);
        let mouse = left_click(geo.editor_area.x + 1, geo.editor_area.y + 1);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_mouse_event(mouse, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let guard = state.lock().await;
        let dialog = guard
            .review
            .refine_dialog
            .as_ref()
            .expect("dialog stays open");
        assert_eq!(
            dialog.focus,
            crate::views::plans::RefineDialogFocus::InstructionsEditor
        );
        assert!(conn.written().is_empty());
    }

    /// Builds a Plans state holding one `Todo` step and an open status dialog,
    /// so a click test can assert both the applied status and the request.
    fn state_with_plan_status_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Plans;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-1".to_string(),
            title: "Test Plan".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
            architect: None,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                files: vec![],
                readonly_files: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        }];
        st.status_dialog = Some(crate::app::StatusDialogState::new(
            crate::app::StatusDialogTarget::PlanItem {
                plan_id: "plan-1".to_string(),
                item_id: "1".to_string(),
                item_title: "Step".to_string(),
                current_status: tauqe_protocol::PlanItemStatus::Todo,
            },
            1,
        ));
        st
    }

    /// A click on a status option must apply the status drawn on that very row.
    ///
    /// The option list starts below the card's top inset and one inter-section
    /// spacer, so the test first renders the dialog, locates the row the
    /// renderer drew for `IN_PROGRESS`, and only then clicks it: the vertical air
    /// must move rendering and hit-testing together (Conventions §6.7, §6.8,
    /// c-1, c-3).
    #[tokio::test]
    async fn status_option_row_click_applies_the_drawn_status() {
        let st = state_with_plan_status_dialog();
        let term = st.last_frame_area;
        let options_count = st
            .status_dialog
            .as_ref()
            .map(|dialog| dialog.target.options_count())
            .unwrap_or(0);
        let options_area = crate::dialogs::render::status_dialog_options_area(term, options_count);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| {
                if let Some(dialog) = st.status_dialog.as_ref() {
                    crate::dialogs::render::render_status_dialog(frame, dialog);
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let drawn_row = (options_area.y..options_area.y + options_area.height)
            .find(|row| {
                (options_area.x..options_area.x + options_area.width)
                    .map(|column| buffer[(column, *row)].symbol())
                    .collect::<String>()
                    .contains("IN_PROGRESS")
            })
            .expect("the IN_PROGRESS option must be drawn inside the options area");

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(options_area.x + 4, drawn_row), &state, &mut conn)
            .await
            .unwrap();

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains(methods::PLAN_UPDATE_ITEM), "{payload}");

        let guard = state.lock().await;
        assert!(guard.status_dialog.is_none(), "the dialog must be consumed");
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.find_item("1"))
            .expect("the plan step must exist");
        assert_eq!(item.status, tauqe_protocol::PlanItemStatus::InProgress);
    }

    /// The reserved air rows of the status dialog are not phantom targets: a
    /// click on the top inset or on the inter-section spacer neither applies a
    /// status nor dismisses the card (Conventions §6.7, c-1).
    #[tokio::test]
    async fn status_air_rows_never_resolve_to_an_option() {
        let st = state_with_plan_status_dialog();
        let term = st.last_frame_area;
        let area = crate::dialogs::render::status_dialog_area(term, 6);
        let options_area = crate::dialogs::render::status_dialog_options_area(term, 6);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        for row in [area.y + 1, options_area.y - 1] {
            handle_mouse_event(left_click(options_area.x + 4, row), &state, &mut conn)
                .await
                .unwrap();
        }

        let guard = state.lock().await;
        assert!(
            guard.status_dialog.is_some(),
            "an air row keeps the dialog open"
        );
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.find_item("1"))
            .expect("the plan step must exist");
        assert_eq!(item.status, tauqe_protocol::PlanItemStatus::Todo);
        assert!(conn.written().is_empty());
    }

    /// The status dialog `Cancel` button dismisses the card without applying the
    /// highlighted status, at the rectangle the shared action bar draws
    /// (Conventions §6.6, c-3).
    #[tokio::test]
    async fn status_cancel_click_dismisses_without_applying() {
        let st = state_with_plan_status_dialog();
        let term = st.last_frame_area;
        let options_count = st
            .status_dialog
            .as_ref()
            .map(|dialog| dialog.target.options_count())
            .unwrap_or(0);
        let (_apply_button, cancel_button) =
            crate::dialogs::render::status_dialog_buttons(term, options_count);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(cancel_button.x + 1, cancel_button.y), &state, &mut conn)
            .await
            .unwrap();

        assert!(conn.written().is_empty());
        let guard = state.lock().await;
        assert!(guard.status_dialog.is_none());
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.find_item("1"))
            .expect("the plan step must exist");
        assert_eq!(item.status, tauqe_protocol::PlanItemStatus::Todo);
    }

    /// Builds a state with an open model picker listing two catalog models.
    fn state_with_selection_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.selection_dialog = Some(crate::app::SelectionDialogState::new(
            crate::app::SelectionDialogKind::ModelCatalog,
            vec![
                crate::app::SelectionItem::Model(tauqe_protocol::ModelRef::openrouter(
                    "alpha-unique",
                )),
                crate::app::SelectionItem::Model(tauqe_protocol::ModelRef::openrouter(
                    "beta-unique",
                )),
            ],
            0,
        ));
        st
    }

    /// Item count of the open picker, read without cloning the dialog.
    fn selection_item_count(st: &crate::app::AppState) -> usize {
        st.selection_dialog
            .as_ref()
            .map(|dialog| dialog.items.len())
            .unwrap_or(0)
    }

    /// A click on the second picker row selects the second model, which proves
    /// the hit-tested list starts exactly where the renderer draws its first
    /// row: the intro line and its spacer sit above the list and are skipped
    /// (Conventions §6.6–§6.8, c-3).
    #[tokio::test]
    async fn selection_list_row_click_activates_the_drawn_row() {
        let st = state_with_selection_dialog();
        let term = st.last_frame_area;
        let list_area = crate::dialogs::render::selection::selection_dialog_list_area(
            term,
            selection_item_count(&st),
        );

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(
            left_click(list_area.x + 3, list_area.y + 1),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains(methods::MODEL_SELECT), "{payload}");
        assert!(payload.contains("beta-unique"), "{payload}");
        assert!(state.lock().await.selection_dialog.is_none());
    }

    /// The picker `Select` button activates the highlighted row at the rectangle
    /// the shared action bar draws (Conventions §6.6, c-3).
    #[tokio::test]
    async fn selection_select_click_activates_the_highlighted_row() {
        let st = state_with_selection_dialog();
        let term = st.last_frame_area;
        let (select_button, _cancel_button) =
            crate::dialogs::render::selection::selection_dialog_buttons(
                term,
                selection_item_count(&st),
            );

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(
            left_click(select_button.x + 2, select_button.y),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains(methods::MODEL_SELECT), "{payload}");
        assert!(payload.contains("alpha-unique"), "{payload}");
        assert!(state.lock().await.selection_dialog.is_none());
    }

    /// The picker `Cancel` button dismisses the dialog without a request.
    #[tokio::test]
    async fn selection_cancel_click_dismisses_without_a_request() {
        let st = state_with_selection_dialog();
        let term = st.last_frame_area;
        let (_select_button, cancel_button) =
            crate::dialogs::render::selection::selection_dialog_buttons(
                term,
                selection_item_count(&st),
            );

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(cancel_button.x + 2, cancel_button.y), &state, &mut conn)
            .await
            .unwrap();

        assert!(conn.written().is_empty());
        assert!(state.lock().await.selection_dialog.is_none());
    }

    /// The intro line above the picker list is not a row: clicking it keeps the
    /// dialog open and starts no model switch (Conventions §6.7, c-1).
    #[tokio::test]
    async fn selection_intro_click_keeps_the_dialog_open() {
        let st = state_with_selection_dialog();
        let term = st.last_frame_area;
        let area = crate::dialogs::render::selection_dialog_area(term, selection_item_count(&st));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        // One row under the top inset: the intro line, above its own spacer.
        handle_mouse_event(left_click(area.x + 3, area.y + 2), &state, &mut conn)
            .await
            .unwrap();

        assert!(conn.written().is_empty());
        assert!(state.lock().await.selection_dialog.is_some());
    }

    /// The first item the picker draws sits exactly on the top row of the
    /// hit-tested list, and the spacer above it is drawn blank, so the vertical
    /// air can never turn into an activatable row (Conventions §6.7, §6.8, c-1).
    #[test]
    fn selection_drawn_rows_align_with_the_hit_tested_list() {
        let st = state_with_selection_dialog();
        let term = st.last_frame_area;
        let item_count = selection_item_count(&st);
        let area = crate::dialogs::render::selection_dialog_area(term, item_count);
        let list_area = crate::dialogs::render::selection::selection_dialog_list_area(term, item_count);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| {
                if let Some(dialog) = st.selection_dialog.as_ref() {
                    crate::dialogs::render::render_selection_dialog(frame, dialog);
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let interior = |row: u16| -> String {
            (area.x + 1..area.x + area.width - 1)
                .map(|column| buffer[(column, row)].symbol())
                .collect()
        };

        assert!(
            list_area.y > area.y + 2,
            "the list starts below the border, the top inset, and the intro"
        );
        assert!(
            interior(list_area.y - 1).trim().is_empty(),
            "the inter-section spacer must stay blank: {:?}",
            interior(list_area.y - 1)
        );
        assert!(
            !interior(list_area.y).trim().is_empty(),
            "the first item must be drawn at the top of the hit-tested list"
        );
    }

    /// The spacer between the refinement list and its editor is inert: a click
    /// there submits nothing and keeps the dialog open (Conventions §6.7, c-1).
    #[tokio::test]
    async fn review_refine_spacer_click_keeps_the_dialog_untouched() {
        let st = state_with_review_refine_dialog();
        let geo = crate::dialogs::render::refine::refine_dialog_geometry(st.last_frame_area, 2);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(
            left_click(geo.editor_area.x + 2, geo.editor_area.y - 1),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        assert!(conn.written().is_empty());
        assert!(state.lock().await.review.refine_dialog.is_some());
    }
}
