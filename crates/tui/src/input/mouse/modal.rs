//! Modal click and scroll routing.
//!
//! Every mouse interaction that targets an open modal lives here: the shared
//! confirmation hit-testing, the wheel routing, and the per-dialog click
//! dispatch for the whole modal stack. The parent module keeps the non-modal
//! surfaces only — header tab switching, viewport scrolling, and code-block
//! copy — so one owner implements the pointer behaviour of the dialogs and the
//! rendered buttons, the hit-tested rectangles, and the dispatched action can
//! never drift apart (Conventions §6.6).

use std::sync::Arc;

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tauqe_protocol::{methods, DiscussionFocus};
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{ActiveModal, AppState, HelpTab, LocaleDialogButton};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::{rect_contains, MOUSE_SCROLL_STEP};

/// Which part of a confirmation modal a pointer landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalHit {
    Confirm,
    Cancel,
    Outside,
    InsideBody,
}

/// Classifies a pointer position against a confirmation modal's geometry.
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
/// [`ConfirmModalParams`](crate::dialogs::render::ConfirmModalParams) the
/// renderer uses, so mouse hit-testing can never drift from what is drawn.
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

/// Routes a wheel event to the modal that owns it.
///
/// Returns `true` when the event was consumed, including the case where a modal
/// is open but holds no scrollable body: background content must never scroll
/// underneath an open dialog.
pub fn handle_modal_scroll(st: &mut AppState, mouse: MouseEvent, term_rect: Rect) -> bool {
    let up = match mouse.kind {
        MouseEventKind::ScrollUp => true,
        MouseEventKind::ScrollDown => false,
        _ => return false,
    };

    if let Some(dialog) = st.status_dialog.as_mut() {
        let count = dialog.target.options_count();
        let visible = crate::dialogs::render::status_dialog_options_area(term_rect, count)
            .height
            .max(1) as usize;
        if up {
            dialog.selector.select_prev(visible);
        } else {
            dialog.selector.select_next(visible);
        }
        return true;
    }

    if let Some(dialog) = st.selection_dialog.as_mut() {
        // The shared selector owns the scroll window, so the wheel moves the
        // highlight exactly like the keyboard and the role dialogs (c-1).
        if up {
            dialog.selector.select_prev();
        } else {
            dialog.selector.select_next();
        }
        return true;
    }

    // The breakdown outranks Help, so its wheel handling is consulted first and
    // the renderer clamps whatever offset it produces (c-4).
    if let Some(summary) = st.usage_summary.as_mut() {
        summary.scroll = if up {
            summary.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16)
        } else {
            summary.scroll.saturating_add(MOUSE_SCROLL_STEP as u16)
        };
        return true;
    }

    if st.show_help {
        st.help_scroll = if up {
            st.help_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16)
        } else {
            st.help_scroll.saturating_add(MOUSE_SCROLL_STEP as u16)
        };
        return true;
    }

    // Allow wheel scrolling in the squash dialog (diff or file list) while it
    // has not escalated to a confirmation.
    if let Some(dialog) = st.squash_dialog.as_mut() {
        if dialog.confirm.is_none() {
            let (_area, files_rect, _diff_rect) =
                crate::dialogs::render::squash_dialog_chunks(term_rect);
            if rect_contains(files_rect, mouse.column, mouse.row) {
                if up {
                    if dialog.selected_file_index > 0 {
                        dialog.selected_file_index -= 1;
                        dialog.diff_scroll = 0;
                    }
                } else if !dialog.files.is_empty()
                    && dialog.selected_file_index + 1 < dialog.files.len()
                {
                    dialog.selected_file_index += 1;
                    dialog.diff_scroll = 0;
                }
            } else {
                dialog.diff_scroll = if up {
                    dialog.diff_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16)
                } else {
                    dialog.diff_scroll.saturating_add(MOUSE_SCROLL_STEP as u16)
                };
            }
            return true;
        }
    }

    st.has_active_modal()
}

/// Routes a left click to the modal that currently owns the keyboard and the
/// screen.
///
/// The dispatch order is the canonical modal ordering, so the surface that
/// renders is always the surface that receives the click. Every rectangle comes
/// from the same geometry helper the renderer draws with, which keeps the drawn
/// buttons and the clickable buttons identical (Conventions §6.6).
pub async fn handle_modal_click(
    mouse: MouseEvent,
    term_rect: Rect,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    match st.active_modal() {
        Some(ActiveModal::ServerDisconnected) => {
            // Single source of truth: the renderer derives the quit button from
            // the same geometry, so clicks and the drawn button can never drift
            // (Conventions §6, §7).
            let geom = crate::dialogs::render::disconnected_button_geometry(term_rect);
            if rect_contains(geom.quit_button, mouse.column, mouse.row)
                || !rect_contains(geom.area, mouse.column, mouse.row)
            {
                return Ok(InputResult::Exit);
            }
        }
        Some(ActiveModal::Squash) => {
            if let Some(dialog) = st.squash_dialog.as_mut() {
                if let Some(kind) = dialog.confirm {
                    // Reuse the exact confirmation parameters the renderer
                    // builds, so mouse hit-testing can never drift from the
                    // drawn buttons (Conventions §6, §7).
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
                                    let params =
                                        tauqe_protocol::GitSquashApplyParams { base_ref, message };
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
                    // Shared with the keyboard launch so a typed target path
                    // rides along on a mouse click too.
                    let params = serde_json::to_value(dialog.start_params())?;
                    crate::dialogs::input::confirm_start_review(state, st, server_writer, params)
                        .await?;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                    || !rect_contains(geo.area, mouse.column, mouse.row)
                {
                    // Safe cancellation: dismiss without restoring.
                    return Ok(InputResult::Continue);
                }

                // Clicking a reviewer row selects it and moves focus to the
                // list, mirroring keyboard navigation.
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
                    // Clicking the target-paths field focuses it so the next
                    // keystroke edits the clicked input.
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
                    dialog.focus = crate::views::plans::RefineDialogFocus::InstructionsEditor;
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
                let geom =
                    geometry_of(term_rect, &crate::dialogs::render::confirm_quit_params(&st));
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
                    crate::dialogs::input::apply_batch_cancel(&mut st, choice, server_writer)
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
                let geom = geometry_of(term_rect, &crate::dialogs::render::clear_auto_params(&st));
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
                let geom = geometry_of(term_rect, &crate::dialogs::render::clear_user_params(&st));
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
                // Reuse the exact confirmation parameters the renderer draws, so
                // hit-testing never desynchronizes from the rendered buttons
                // (Conventions §6, §7).
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::clear_horizon_params(&st, &plan_id),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_clear_horizon = None;
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        // A horizon that already carries started or completed
                        // steps has committed work in Git: the reset escalates to
                        // the second tier instead of dropping recorded history
                        // silently.
                        let recorded = st
                            .plans_view
                            .plans
                            .iter()
                            .find(|p| p.id == plan_id)
                            .is_some_and(|p| p.has_started_or_completed_steps());
                        if recorded {
                            st.confirm_clear_horizon_final = Some(plan_id);
                        } else {
                            // The success notice is emitted once the server
                            // confirms the reset in the response handler, not on
                            // the click path.
                            drop(st);
                            crate::dialogs::input::clear_plan_horizon(server_writer, plan_id)
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
                        crate::dialogs::input::clear_plan_horizon(server_writer, plan_id).await?;
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
                        // A plan that still carries unfinished work escalates to
                        // the second tier exactly as pressing `Enter` does, so a
                        // click can never discard progress silently (c-1).
                        if crate::dialogs::input::plan_delete_needs_escalation(&st, &plan_id) {
                            st.confirm_delete_plan_final = Some(plan_id);
                        } else {
                            drop(st);
                            crate::dialogs::input::delete_plan(server_writer, plan_id).await?;
                        }
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_delete_plan = None;
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::DeletePlanFinal) => {
            if let Some(plan_id) = st.confirm_delete_plan_final.clone() {
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::delete_plan_final_params(&st, &plan_id),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_delete_plan_final = None;
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        drop(st);
                        crate::dialogs::input::delete_plan(server_writer, plan_id).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_delete_plan_final = None;
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
                        crate::dialogs::input::delete_review(server_writer, review_id).await?;
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
            // Point execution draws the Coder form dialog, whose action buttons,
            // model rows, and editor area come from one geometry source shared
            // with the renderer (Conventions §6, §7).
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
                        target.focus = crate::views::plans::RefineDialogFocus::InstructionsEditor;
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
            // confirmation is the binary confirm/cancel card whose geometry is
            // derived from the same parameters the renderer draws
            // (Conventions §6, §7).
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
                    crate::dialogs::input::submit_refine_dialog(state, st, dialog, server_writer)
                        .await?;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                    || !rect_contains(geo.area, mouse.column, mouse.row)
                {
                    // Safe cancellation: dismiss without starting a turn.
                    return Ok(InputResult::Continue);
                }

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
                    // Clicking the instructions editor focuses it so the next
                    // keystroke edits the clicked input.
                    dialog.focus = crate::views::plans::RefineDialogFocus::InstructionsEditor;
                }

                st.plans_view.refine_dialog = Some(dialog);
            }
        }
        Some(ActiveModal::RefineBlocked) => {
            if let Some(blocked) = st.plans_view.refine_blocked.clone() {
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::refine_blocked_params(&blocked, st.confirm_button),
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
            // Confirm submits the comment exactly as `Enter` does; Cancel and
            // any click outside dismiss the modal safely.
            let (confirm_btn, cancel_btn) = crate::dialogs::render::prompt_modal_buttons(term_rect);
            if rect_contains(confirm_btn, mouse.column, mouse.row) {
                if let Some(dialog) = st.prompt_modal.take() {
                    let _ = crate::dialogs::input::submit_prompt_modal(st, dialog, server_writer)
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
            // Clicking a preset previews it; clicking outside cancels, restoring
            // the preset that was active on open.
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
                        crate::dialogs::input::confirm_clear_history(st, server_writer).await?;
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
                let area = crate::dialogs::render::status_dialog_area(term_rect, options_count);
                let options_area =
                    crate::dialogs::render::status_dialog_options_area(term_rect, options_count);
                let (apply_btn, cancel_btn) =
                    crate::dialogs::render::status_dialog_buttons(term_rect, options_count);

                // The action bar carries two distinct buttons: Apply commits the
                // highlighted status, Cancel dismisses. Hit each button by its
                // rendered bounds so a click on Apply can never fall through to
                // the dismiss path.
                if rect_contains(apply_btn, mouse.column, mouse.row) {
                    let chosen = dialog.selector.selected_index();
                    crate::dialogs::input::apply_status_dialog(&mut st, chosen, server_writer)
                        .await?;
                } else if rect_contains(cancel_btn, mouse.column, mouse.row) {
                    st.status_dialog = None;
                } else if rect_contains(options_area, mouse.column, mouse.row) {
                    // Map the clicked visual row onto the list index, honoring
                    // the current scroll offset and the exact window the rows are
                    // drawn in, so a click on empty space below the last option
                    // never selects a status (Conventions §6.6).
                    let clicked_row =
                        dialog.selector.scroll_offset() + (mouse.row - options_area.y) as usize;
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
            let item_count = st
                .selection_dialog
                .as_ref()
                .map(|dialog| dialog.selector.len())
                .unwrap_or(0);
            let area = crate::dialogs::render::selection_dialog_area(term_rect, item_count);
            let (select_btn, cancel_btn) =
                crate::dialogs::render::selection::selection_dialog_buttons(term_rect, item_count);

            // Action footer: Select activates the highlighted rung, Cancel
            // dismisses.
            if rect_contains(select_btn, mouse.column, mouse.row) {
                let selected = st
                    .selection_dialog
                    .as_ref()
                    .and_then(|dialog| dialog.selector.selected_entry())
                    .cloned();
                if let Some(entry) = selected {
                    crate::dialogs::input::activate_selection_item(st, entry, server_writer)
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

            // Rows resolve through the same list rectangle the renderer draws,
            // so the air above the first rung and below the last one never
            // activates a model (Conventions §6.6, §6.7, c-3).
            let list_area = crate::dialogs::render::selection::selection_dialog_list_area(
                term_rect, item_count,
            );
            if rect_contains(list_area, mouse.column, mouse.row) {
                let clicked_visible = (mouse.row - list_area.y) as usize;
                // The click is translated with the selector's own scroll
                // arithmetic, so hit-testing and the drawn window cannot drift
                // apart (c-1).
                let clicked = st.selection_dialog.as_mut().and_then(|dialog| {
                    let visible_height = list_area.height.max(1) as usize;
                    let index = dialog.selector.visible_start(visible_height) + clicked_visible;
                    if index < dialog.selector.len() {
                        dialog.selector.select_index(index);
                        dialog.selector.selected_entry().cloned()
                    } else {
                        None
                    }
                });
                if let Some(entry) = clicked {
                    crate::dialogs::input::activate_selection_item(st, entry, server_writer)
                        .await?;
                }
            }
        }
        Some(ActiveModal::Locale) => {
            // The three action buttons are clickable through the same geometry
            // the renderer draws; a click outside the dialog dismisses it and a
            // click elsewhere inside leaves the language field to the keyboard
            // (Conventions §6.6).
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
                crate::dialogs::input::click_locale_button(st, button, server_writer).await?;
                return Ok(InputResult::Continue);
            }
            if !rect_contains(geom.area, mouse.column, mouse.row) {
                st.locale_dialog = None;
            }
        }
        Some(ActiveModal::UsageSummary) => {
            // The breakdown exposes no controls: a click outside its frame
            // dismisses it and a click inside leaves it open for the wheel and
            // the keyboard (Conventions §6).
            let area = crate::dialogs::render::usage_summary_area(term_rect);
            if !rect_contains(area, mouse.column, mouse.row) {
                st.usage_summary = None;
            }
        }
        Some(ActiveModal::Help) => {
            // A click on a header tab switches that tab; any other click
            // dismisses the dialog, preserving the click-to-close affordance
            // while making the tabs reachable (Conventions §6.6).
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
        None => {}
    }

    Ok(InputResult::Continue)
}
