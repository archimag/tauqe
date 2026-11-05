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
use crate::dialogs::input::activate_selection_item;
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
        &params.body_lines,
        &params.confirm_label,
        &params.cancel_label,
    )
}

/// Routes a wheel event to the modal that owns it.
///
/// Returns `true` when the event was consumed, including the case where a modal
/// is open but holds no scrollable body: background content must never scroll
/// underneath an open dialog.
///
/// The dispatch reads the canonical modal stack that rendering, keyboard
/// dispatch, and hit-testing already share, so the surface drawn on top is the
/// surface that receives the notch, and a surface added to [`ActiveModal`]
/// cannot silently inherit the wheel behaviour of another one (Conventions §6,
/// c-1).
pub fn handle_modal_scroll(st: &mut AppState, mouse: MouseEvent, term_rect: Rect) -> bool {
    if !matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        return false;
    }

    match st.active_modal() {
        // The inline Context path input is the one surface outside the modal
        // stack: it keeps the wheel for itself so the list behind it cannot move
        // while a path is typed into it (Conventions §5.1).
        None => st.context_view.adding_file,
        // A notch is always consumed by the modal that owns the keyboard, even
        // when its body cannot move, so no background surface scrolls under an
        // open card (Conventions §6).
        Some(modal) => {
            scroll_modal_surface(st, modal, mouse, term_rect);
            true
        }
    }
}

/// Applies one wheel notch to the body of `modal`.
///
/// The match is exhaustive and carries no wildcard: every [`ActiveModal`]
/// variant states what a notch does to it, and the surfaces without a
/// scrollable body state that the notch moves nothing. A newly declared surface
/// therefore cannot compile until its wheel behaviour is written down, so the
/// routing never falls back to a wildcard arm (c-1).
fn scroll_modal_surface(st: &mut AppState, modal: ActiveModal, mouse: MouseEvent, term_rect: Rect) {
    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
    match modal {
        ActiveModal::OuterDocsCache => {
            // The browser mirrors the keyboard: one notch walks the tree by the
            // shared step, and the highlight is re-anchored afterwards so the
            // row it landed on stays visible.
            if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                dialog.move_cursor(if up {
                    -(MOUSE_SCROLL_STEP as isize)
                } else {
                    MOUSE_SCROLL_STEP as isize
                });
                let visible = crate::dialogs::render::outer_docs_cache_visible_rows(term_rect);
                dialog.scroll_to_cursor(visible);
            }
        }
        ActiveModal::Status => {
            // The picker scrolls a window of status rows, so one notch pages the
            // highlight exactly like `PgUp` / `PgDn`.
            if let Some(dialog) = st.modals.status_dialog.as_mut() {
                let count = dialog.target.options_count();
                let visible = crate::dialogs::render::status_dialog_options_area(term_rect, count)
                    .height
                    .max(1) as usize;
                if up {
                    dialog.selector.select_prev(visible);
                } else {
                    dialog.selector.select_next(visible);
                }
            }
        }
        ActiveModal::Selection => {
            // The shared model selector owns the scroll window, so the wheel
            // moves the highlight exactly like the keyboard and the role dialogs
            // (c-1).
            if let Some(dialog) = st.modals.selection_dialog.as_mut() {
                if up {
                    dialog.selector.select_prev();
                } else {
                    dialog.selector.select_next();
                }
            }
        }
        ActiveModal::UsageSummary => {
            // The breakdown outranks Help, and the renderer clamps whatever
            // offset the notch produces (c-4).
            if let Some(summary) = st.modals.usage_summary.as_mut() {
                summary.scroll = if up {
                    summary.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16)
                } else {
                    summary.scroll.saturating_add(MOUSE_SCROLL_STEP as u16)
                };
            }
        }
        ActiveModal::Help => {
            st.modals.help_scroll = if up {
                st.modals
                    .help_scroll
                    .saturating_sub(MOUSE_SCROLL_STEP as u16)
            } else {
                st.modals
                    .help_scroll
                    .saturating_add(MOUSE_SCROLL_STEP as u16)
            };
        }
        ActiveModal::Squash => {
            // An escalated squash card is a decision, not a scrollable surface.
            // Otherwise the wheel walks the file list under the pointer and
            // scrolls the diff beside it.
            if let Some(dialog) = st.modals.squash_dialog.as_mut() {
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
                }
            }
        }
        // Every remaining surface owns a body a notch cannot move: the card
        // states a decision, a summary, or a single-line field, so the notch is
        // consumed and the view underneath it keeps its reading position
        // (Conventions §6). `DiffInspector` keeps its reading position to its own
        // chords, and `OuterDocsCacheRemove` is the pending decision that
        // outranks the tree it was raised from.
        ActiveModal::ServerDisconnected
        | ActiveModal::ReviewTruncation
        | ActiveModal::Review
        | ActiveModal::ConfirmCancel
        | ActiveModal::ConfirmQuit
        | ActiveModal::BatchCancel
        | ActiveModal::ClearAuto
        | ActiveModal::ClearUser
        | ActiveModal::RemoveContextFile
        | ActiveModal::OuterDocsCacheRemove
        | ActiveModal::ConfirmDetachFocus
        | ActiveModal::ClearHorizonFinal
        | ActiveModal::ClearHorizon
        | ActiveModal::DeletePlanFinal
        | ActiveModal::DeletePlan
        | ActiveModal::DeleteReview
        | ActiveModal::ReviewAnalyze
        | ActiveModal::ExecuteReview
        | ActiveModal::ExecutePipeline
        | ActiveModal::ExecuteScope
        | ActiveModal::CreatePlan
        | ActiveModal::RefineDialog
        | ActiveModal::RefineBlocked
        | ActiveModal::Prompt
        | ActiveModal::Locale
        | ActiveModal::ClearHistory
        | ActiveModal::DiffInspector
        | ActiveModal::ThemeSelector => {}
    }
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
            if let Some(dialog) = st.modals.squash_dialog.as_mut() {
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
                                    st.modals.squash_dialog = None;
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
                        st.modals.squash_dialog = None;
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
            if let Some(mut dialog) = st.modals.review_dialog.take() {
                let geo = crate::dialogs::render::review_dialog_geometry(term_rect, &dialog);

                if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                    // Shared with the keyboard launch, so a click honours the
                    // live match gate: a target pattern that selects no
                    // repository file is refused with a warning, the dialog
                    // keeps its fields, and no request reaches the daemon
                    // (c-2, c-4).
                    crate::dialogs::input::start_review_from_dialog(
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
                } else if rect_contains(geo.preset_rows, mouse.column, mouse.row) {
                    // Clicking a preset row selects it and hands the keyboard to
                    // the section, exactly like the reviewer rows above. The
                    // click is translated with the selector's own scroll
                    // arithmetic and the same row rectangle the renderer draws,
                    // so hit-testing and the drawn window cannot drift apart
                    // (c-5).
                    let visible = geo.preset_rows.height as usize;
                    let start = dialog.preset_selector.scroll_offset();
                    let clicked_row = (mouse.row - geo.preset_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.preset_selector.item_count() {
                        dialog.preset_selector.select_index(index, visible);
                        dialog.focus = crate::app::ReviewDialogFocus::PresetList;
                    }
                } else if rect_contains(geo.completions, mouse.column, mouse.row) {
                    // Clicking a completion row accepts it into the active path
                    // row, exactly as `Tab` accepts the highlighted candidate,
                    // and the row is translated through the completion's own
                    // scroll arithmetic so hit-testing and the drawn window
                    // cannot drift apart (c-5).
                    let visible = geo.completions.height.saturating_sub(2) as usize;
                    let start = dialog.path_completion.visible_start(visible);
                    let clicked_row = (mouse.row - geo.completions.y).saturating_sub(1) as usize;
                    let index = start + clicked_row;
                    if index < dialog.path_completion.candidates().len() {
                        dialog.accept_path_completion_at(index);
                    }
                } else if rect_contains(geo.path_rows, mouse.column, mouse.row) {
                    // Clicking a path row focuses that very row, so the keyboard
                    // continues exactly where the developer pointed (c-3).
                    let visible = geo.path_rows.height as usize;
                    let start = dialog.path_visible_start(visible);
                    let clicked_row = (mouse.row - geo.path_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.path_row_count() {
                        dialog.path_active = index;
                        dialog.refresh_path_completion();
                    }
                    dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
                } else if rect_contains(geo.path_area, mouse.column, mouse.row) {
                    // Clicking the frame or the counter row focuses the section
                    // without moving the active row.
                    dialog.focus = crate::app::ReviewDialogFocus::PathEditor;
                } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                    dialog.focus = crate::app::ReviewDialogFocus::PromptEditor;
                }

                st.modals.review_dialog = Some(dialog);
            }
        }
        Some(ActiveModal::ConfirmCancel) => {
            if st.modals.confirm_cancel {
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::confirm_cancel_params(&st),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_cancel = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                        st.modals.confirm_cancel = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ConfirmQuit) => {
            if st.modals.confirm_quit {
                let geom =
                    geometry_of(term_rect, &crate::dialogs::render::confirm_quit_params(&st));
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_quit = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        return Ok(InputResult::Exit);
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_quit = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                    // The guard travels by value: the helper releases it before
                    // the cancellation requests leave the loop (c-1).
                    crate::dialogs::input::apply_batch_cancel(st, choice, server_writer).await?;
                }
                None => {
                    st.modals.confirm_batch_cancel = false;
                    st.modals.batch_cancel_button = crate::app::BatchCancelButton::Continue;
                }
            }
            return Ok(InputResult::Continue);
        }
        Some(ActiveModal::ClearAuto) => {
            if st.modals.confirm_clear_auto {
                let geom = geometry_of(term_rect, &crate::dialogs::render::clear_auto_params(&st));
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_clear_auto = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                        st.modals.confirm_clear_auto = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ClearUser) => {
            if st.modals.confirm_clear_user {
                let geom = geometry_of(term_rect, &crate::dialogs::render::clear_user_params(&st));
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_clear_user = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                        st.modals.confirm_clear_user = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::OuterDocsCacheRemove) => {
            if let Some(path) = st.modals.confirm_outer_docs_cache_remove.clone() {
                let geom = match crate::dialogs::render::outer_docs_cache_remove_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_outer_docs_cache_remove = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        if let Some(dialog) = st.modals.outer_docs_cache.as_mut() {
                            dialog.loading = true;
                        }
                        drop(st);
                        let params = tauqe_protocol::OuterDocsCacheRemoveParams { path };
                        send_request(
                            server_writer,
                            methods::OUTER_DOCS_CACHE_REMOVE,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_outer_docs_cache_remove = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::OuterDocsCache) => {
            // The pointer mirrors the keyboard: a click highlights the row it
            // lands on, and a second click on the highlighted directory folds
            // it. The row is translated through the very tree pane the renderer
            // draws with, so the two can never disagree (Conventions §6.6).
            if let Some(mut dialog) = st.modals.outer_docs_cache.take() {
                let area = crate::dialogs::render::outer_docs_cache_area(term_rect);
                let tree = crate::dialogs::render::outer_docs_cache_tree_rect(term_rect);
                if rect_contains(tree, mouse.column, mouse.row) {
                    let clicked = usize::from(dialog.scroll) + usize::from(mouse.row - tree.y);
                    if clicked < dialog.rows().len() {
                        if clicked == dialog.cursor {
                            dialog.toggle_fold();
                        } else {
                            dialog.cursor = clicked;
                        }
                    }
                } else if !rect_contains(area, mouse.column, mouse.row) {
                    // A click outside the card dismisses the browser and sends
                    // nothing (Conventions §6).
                    return Ok(InputResult::Continue);
                }
                let visible = usize::from(tree.height).max(1);
                dialog.scroll_to_cursor(visible);
                st.modals.outer_docs_cache = Some(dialog);
            }
            return Ok(InputResult::Continue);
        }
        Some(ActiveModal::RemoveContextFile) => {
            if st.modals.confirm_remove_context_file.is_some() {
                let geom = match crate::dialogs::render::remove_context_file_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        if let Some(path) = st.modals.confirm_remove_context_file.take() {
                            st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                            drop(st);
                            let params = tauqe_protocol::ContextRemoveParams { path };
                            send_request(
                                server_writer,
                                methods::CONTEXT_REMOVE,
                                serde_json::to_value(params)?,
                            )
                            .await?;
                        }
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_remove_context_file = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ConfirmDetachFocus) => {
            if st.modals.confirm_detach_focus {
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::confirm_detach_focus_params(&st),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_detach_focus = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                        st.modals.confirm_detach_focus = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ClearHorizon) => {
            if let Some(plan_id) = st.modals.confirm_clear_horizon.clone() {
                // Reuse the exact confirmation parameters the renderer draws, so
                // hit-testing never desynchronizes from the rendered buttons
                // (Conventions §6, §7).
                let geom = match crate::dialogs::render::clear_horizon_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_clear_horizon = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        // A horizon that already carries started or completed
                        // steps has committed work in Git: the reset escalates to
                        // the second tier instead of dropping recorded history
                        // silently.
                        let recorded = st
                            .plans_view
                            .plans
                            .iter()
                            .find(|p| p.id == plan_id)
                            .is_some_and(|p| p.implementation.has_started_or_completed_steps());
                        if recorded {
                            st.modals.confirm_clear_horizon_final = Some(plan_id);
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
                        st.modals.confirm_clear_horizon = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ClearHorizonFinal) => {
            if let Some(plan_id) = st.modals.confirm_clear_horizon_final.clone() {
                let geom = match crate::dialogs::render::clear_horizon_final_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_clear_horizon_final = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        drop(st);
                        crate::dialogs::input::clear_plan_horizon(server_writer, plan_id).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_clear_horizon_final = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::DeletePlan) => {
            if let Some(plan_id) = st.modals.confirm_delete_plan.clone() {
                let geom = match crate::dialogs::render::delete_plan_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_delete_plan = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        // A plan that still carries unfinished work escalates to
                        // the second tier exactly as pressing `Enter` does, so a
                        // click can never discard progress silently (c-1).
                        if crate::dialogs::input::plan_delete_needs_escalation(&st, &plan_id) {
                            st.modals.confirm_delete_plan_final = Some(plan_id);
                        } else {
                            drop(st);
                            crate::dialogs::input::delete_plan(server_writer, plan_id).await?;
                        }
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_delete_plan = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::DeletePlanFinal) => {
            if let Some(plan_id) = st.modals.confirm_delete_plan_final.clone() {
                let geom = match crate::dialogs::render::delete_plan_final_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_delete_plan_final = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        drop(st);
                        crate::dialogs::input::delete_plan(server_writer, plan_id).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_delete_plan_final = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::DeleteReview) => {
            if let Some(review_id) = st.modals.confirm_delete_review.clone() {
                let geom = match crate::dialogs::render::delete_review_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_delete_review = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        drop(st);
                        crate::dialogs::input::delete_review(server_writer, review_id).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_delete_review = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ReviewAnalyze) => {
            // The post-analysis dialog repeats the execution-dialog shape: the
            // Architect ladder above the action bar, whose rows and buttons come
            // from one geometry source shared with the renderer (Conventions §6,
            // §7, c-dialog-parity).
            if let Some(mut target) = st.modals.confirm_review_analyze.take() {
                let geo = crate::dialogs::render::review_analyze_dialog_geometry(
                    term_rect,
                    target.selector.len(),
                );

                if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    crate::dialogs::input::confirm_review_analyze(state, st, target, server_writer)
                        .await?;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                    || !rect_contains(geo.area, mouse.column, mouse.row)
                {
                    // Safe cancellation: dismiss without spending tokens.
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                    target.focus = crate::app::ReviewAnalyzeFocus::ModelList;
                }

                st.modals.confirm_review_analyze = Some(target);
            }
            return Ok(InputResult::Continue);
        }
        Some(ActiveModal::ReviewTruncation) => {
            // The consent card reuses the exact confirmation parameters the
            // renderer draws, so hit-testing never desynchronizes from the
            // rendered buttons (Conventions §6, §7).
            let geom = match crate::dialogs::render::review_truncation_params(&st) {
                Some(params) => geometry_of(term_rect, &params),
                None => return Ok(InputResult::Continue),
            };
            if let Some(target) = st.modals.confirm_review_truncation.take() {
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        crate::dialogs::input::launch_review(
                            state,
                            st,
                            server_writer,
                            target.pending.params,
                        )
                        .await?;
                        return Ok(InputResult::Continue);
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        // The safe default returns the launch dialog with its
                        // typed fields and sends nothing (c-3).
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        st.modals.review_dialog = Some(target.pending.dialog);
                    }
                    ModalHit::InsideBody => st.modals.confirm_review_truncation = Some(target),
                }
            }
        }
        Some(ActiveModal::ExecuteReview) => {
            // Point execution draws the Coder form dialog, whose action buttons,
            // model rows, and editor area come from one geometry source shared
            // with the renderer (Conventions §6, §7).
            let single_target = st
                .modals
                .confirm_execute_review
                .as_ref()
                .is_some_and(|target| target.is_single_target());
            if single_target {
                if let Some(mut target) = st.modals.confirm_execute_review.take() {
                    let geo = crate::dialogs::render::execute_review_dialog_geometry(
                        term_rect,
                        target.selector.len(),
                    );

                    if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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

                    st.modals.confirm_execute_review = Some(target);
                }
                return Ok(InputResult::Continue);
            }
            if let Some(target) = st.modals.confirm_execute_review.clone() {
                let geom = match crate::dialogs::render::execute_review_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_execute_review = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        crate::dialogs::input::confirm_execute_review(
                            state,
                            st,
                            server_writer,
                            target,
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_execute_review = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
            if let Some(target) = st.modals.confirm_execute_pipeline.clone() {
                let geom = match crate::dialogs::render::execute_pipeline_params(&st) {
                    Some(params) => geometry_of(term_rect, &params),
                    None => return Ok(InputResult::Continue),
                };
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_execute_pipeline = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        crate::dialogs::input::confirm_execute_pipeline(
                            state,
                            st,
                            server_writer,
                            target,
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_execute_pipeline = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::ExecuteScope) => {
            // Executing a step or a whole scope draws the Coder confirmation,
            // whose action buttons and model rows come from one geometry source
            // shared with the renderer (Conventions §6, §7).
            if let Some(mut target) = st.modals.confirm_execute_scope.take() {
                let geo = crate::dialogs::render::execute_scope_dialog_geometry(
                    term_rect,
                    target.selector.len(),
                    target.steps.len() == 1,
                );

                if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    crate::dialogs::input::confirm_execute_scope(state, st, server_writer, target)
                        .await?;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                    || !rect_contains(geo.area, mouse.column, mouse.row)
                {
                    // Safe cancellation: dismiss without a turn.
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                    target.focus = crate::app::ExecuteScopeFocus::ModelList;
                }

                st.modals.confirm_execute_scope = Some(target);
            }
            return Ok(InputResult::Continue);
        }
        Some(ActiveModal::CreatePlan) => {
            // The creation form stacks the Planner ladder, the Critic ladder,
            // the directives editor, and the action bar. Rows and buttons come
            // from the same geometry the renderer draws with (Conventions §6.6).
            if let Some(mut dialog) = st.modals.create_dialog.take() {
                let geo = crate::dialogs::render::create_plan_dialog_geometry(
                    term_rect,
                    dialog.planner.len(),
                    dialog.critic.len(),
                );

                if rect_contains(geo.confirm_button, mouse.column, mouse.row) {
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    crate::dialogs::input::submit_create_dialog(state, st, dialog, server_writer)
                        .await?;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.cancel_button, mouse.column, mouse.row)
                    || !rect_contains(geo.area, mouse.column, mouse.row)
                {
                    // Safe cancellation: dismiss without a turn.
                    st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    return Ok(InputResult::Continue);
                }

                if rect_contains(geo.planner_rows, mouse.column, mouse.row) {
                    let visible = geo.planner_rows.height as usize;
                    let start = dialog.planner.visible_start(visible);
                    let clicked_row = (mouse.row - geo.planner_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.planner.len() {
                        dialog.planner.select_index(index);
                    }
                    dialog.focus = crate::views::plans::PlanFormFocus::PlannerList;
                } else if rect_contains(geo.critic_rows, mouse.column, mouse.row) {
                    let visible = geo.critic_rows.height as usize;
                    let start = dialog.critic.visible_start(visible);
                    let clicked_row = (mouse.row - geo.critic_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.critic.len() {
                        dialog.critic.select_index(index);
                    }
                    dialog.focus = crate::views::plans::PlanFormFocus::CriticList;
                } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                    dialog.focus = crate::views::plans::PlanFormFocus::InstructionsEditor;
                }

                st.modals.create_dialog = Some(dialog);
            }
            return Ok(InputResult::Continue);
        }
        Some(ActiveModal::RefineDialog) => {
            if let Some(mut dialog) = st.modals.refine_dialog.take() {
                let geo = crate::dialogs::render::refine_dialog_geometry(
                    term_rect,
                    dialog.planner.len(),
                    dialog.critic.len(),
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

                if rect_contains(geo.planner_rows, mouse.column, mouse.row) {
                    let visible = geo.planner_rows.height as usize;
                    let start = dialog.planner.visible_start(visible);
                    let clicked_row = (mouse.row - geo.planner_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.planner.len() {
                        dialog.planner.select_index(index);
                    }
                    dialog.focus = crate::views::plans::PlanFormFocus::PlannerList;
                } else if rect_contains(geo.critic_rows, mouse.column, mouse.row) {
                    let visible = geo.critic_rows.height as usize;
                    let start = dialog.critic.visible_start(visible);
                    let clicked_row = (mouse.row - geo.critic_rows.y) as usize;
                    let index = start + clicked_row;
                    if index < dialog.critic.len() {
                        dialog.critic.select_index(index);
                    }
                    dialog.focus = crate::views::plans::PlanFormFocus::CriticList;
                } else if rect_contains(geo.editor_area, mouse.column, mouse.row) {
                    dialog.focus = crate::views::plans::PlanFormFocus::InstructionsEditor;
                }

                st.modals.refine_dialog = Some(dialog);
            }
        }
        Some(ActiveModal::RefineBlocked) => {
            if let Some(blocked) = st.modals.refine_blocked.clone() {
                let confirm_button = st.modals.confirm_button;
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::refine_blocked_params(&blocked, confirm_button),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        crate::dialogs::input::submit_refine_blocked(
                            state,
                            st,
                            blocked,
                            server_writer,
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.refine_blocked = None;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
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
                if let Some(dialog) = st.modals.prompt_modal.take() {
                    // The guard travels by value so the launch transition can
                    // release it before the comment is dispatched (c-1).
                    let _ = crate::dialogs::input::submit_prompt_modal(
                        state,
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
                st.modals.prompt_modal = None;
            }
        }
        Some(ActiveModal::ThemeSelector) => {
            // Clicking a preset previews it; clicking outside cancels, restoring
            // the preset that was active on open. The selector releases the guard
            // around the `tui.toml` rewrite, so a click never parks the event loop
            // on the disk (c-1).
            crate::theme::selector::handle_theme_selector_click(
                state,
                st,
                mouse.column,
                mouse.row,
                term_rect,
            )
            .await;
        }
        Some(ActiveModal::ClearHistory) => {
            if st.modals.confirm_clear_history {
                let geom = geometry_of(
                    term_rect,
                    &crate::dialogs::render::clear_history_params(&st),
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.modals.confirm_clear_history = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        crate::dialogs::input::confirm_clear_history(st, server_writer).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.modals.confirm_clear_history = false;
                        st.modals.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                    }
                    ModalHit::InsideBody => {}
                }
            }
        }
        Some(ActiveModal::Status) => {
            if let Some(dialog) = st.modals.status_dialog.clone() {
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
                    // The guard travels by value: the helper reverts the
                    // optimistic mutation at the site that owns it when the
                    // write fails, and releases the guard before the request
                    // leaves the loop (c-1, c-3).
                    crate::dialogs::input::apply_status_dialog(state, st, chosen, server_writer)
                        .await?;
                } else if rect_contains(cancel_btn, mouse.column, mouse.row) {
                    st.modals.status_dialog = None;
                } else if rect_contains(options_area, mouse.column, mouse.row) {
                    // Map the clicked visual row onto the list index, honoring
                    // the current scroll offset and the exact window the rows are
                    // drawn in, so a click on empty space below the last option
                    // never selects a status (Conventions §6.6).
                    let clicked_row =
                        dialog.selector.scroll_offset() + (mouse.row - options_area.y) as usize;
                    if clicked_row < options_count {
                        crate::dialogs::input::apply_status_dialog(
                            state,
                            st,
                            clicked_row,
                            server_writer,
                        )
                        .await?;
                    }
                } else if !rect_contains(area, mouse.column, mouse.row) {
                    st.modals.status_dialog = None;
                }
            }
        }
        Some(ActiveModal::Selection) => {
            let item_count = st
                .modals
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
                    .modals
                    .selection_dialog
                    .as_ref()
                    .and_then(|dialog| dialog.selector.selected_entry())
                    .cloned();
                if let Some(entry) = selected {
                    activate_selection_item(state, st, entry, server_writer).await?;
                }
                return Ok(InputResult::Continue);
            }
            if rect_contains(cancel_btn, mouse.column, mouse.row) {
                st.modals.selection_dialog = None;
                return Ok(InputResult::Continue);
            }

            if !rect_contains(area, mouse.column, mouse.row) {
                st.modals.selection_dialog = None;
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
                let clicked = st.modals.selection_dialog.as_mut().and_then(|dialog| {
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
                    activate_selection_item(state, st, entry, server_writer).await?;
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
                crate::dialogs::input::click_locale_button(state, st, button, server_writer)
                    .await?;
                return Ok(InputResult::Continue);
            }
            if !rect_contains(geom.area, mouse.column, mouse.row) {
                st.modals.locale_dialog = None;
            }
        }
        Some(ActiveModal::UsageSummary) => {
            // The breakdown exposes no controls: a click outside its frame
            // dismisses it and a click inside leaves it open for the wheel and
            // the keyboard (Conventions §6).
            let area = crate::dialogs::render::usage_summary_area(term_rect);
            if !rect_contains(area, mouse.column, mouse.row) {
                st.modals.usage_summary = None;
            }
        }
        Some(ActiveModal::Help) => {
            // A click on a header tab switches that tab; any other click
            // dismisses the dialog, preserving the click-to-close affordance
            // while making the tabs reachable (Conventions §6.6).
            let geom = crate::dialogs::render::help::help_tab_geometry(term_rect);
            let help = &mut st.modals;
            if rect_contains(geom.global_tab, mouse.column, mouse.row) {
                help.help_tab = HelpTab::Global;
                help.help_scroll = 0;
            } else if rect_contains(geom.current_mode_tab, mouse.column, mouse.row) {
                help.help_tab = HelpTab::CurrentMode;
                help.help_scroll = 0;
            } else if rect_contains(geom.text_editor_tab, mouse.column, mouse.row) {
                help.help_tab = HelpTab::TextEditor;
                help.help_scroll = 0;
            } else {
                help.show_help = false;
                help.help_scroll = 0;
            }
        }
        // The inspector is a reading surface whose pointer contract mirrors its
        // keyboard: a click on an audited file selects it and hands the file list
        // the keyboard, a click in the reading area focuses the patch, and a
        // click outside the card dismisses the audit. Both rectangles come from
        // the geometry the renderer draws with, so the surface that is drawn is
        // the surface that is clicked and the row window the renderer scrolled to
        // is the window a click resolves against (Conventions §6.6, c-3).
        Some(ActiveModal::DiffInspector) => {
            let geometry = crate::dialogs::render::diff_inspector_geometry(term_rect);
            if !rect_contains(geometry.area, mouse.column, mouse.row) {
                // Dismissal never keeps an imported commit change set alive: the
                // next open starts from the live staged set unless it imports its
                // own (c-3).
                st.modals.diff_audit = None;
                st.modals.diff_inspector = None;
                return Ok(InputResult::Continue);
            }

            // The audited set is either the imported commit audit or the live
            // staged set the viewport owns, so its length is read from the active
            // source before the inspector is borrowed (c-3).
            let file_count = match &st.modals.diff_audit {
                Some(audit) => audit.files.len(),
                None => st.model.files.len(),
            };
            if let Some(inspector) = st.modals.diff_inspector.as_mut() {
                if rect_contains(geometry.files, mouse.column, mouse.row) {
                    // The click skips the border row and is translated through the
                    // same window the renderer scrolled to, so a click below the
                    // last audited file selects nothing and a deeper row can never
                    // name a file the pane does not draw (Conventions §6.6, c-3).
                    let visible = usize::from(geometry.files.height.saturating_sub(2)).max(1);
                    let offset = (inspector.selected_file + 1).saturating_sub(visible);
                    let clicked_row =
                        usize::from(mouse.row.saturating_sub(geometry.files.y.saturating_add(1)));
                    let index = offset.saturating_add(clicked_row);
                    if index < file_count && index != inspector.selected_file {
                        inspector.selected_file = index;
                        // A new file opens at its first line, so the patch below
                        // never inherits the offset of the file it replaced (c-2).
                        inspector.diff_scroll = 0;
                    }
                    inspector.focus = crate::app::DiffInspectorFocus::Files;
                } else if rect_contains(geometry.diff, mouse.column, mouse.row) {
                    // The reading area owns the vertical motion once it holds the
                    // keyboard, exactly as `Tab` hands it over (c-2).
                    inspector.focus = crate::app::DiffInspectorFocus::Diff;
                }
            }
        }
        None => {}
    }

    Ok(InputResult::Continue)
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton};
    use ratatui::layout::Rect;

    use super::*;
    use crate::app::{
        ConfirmDialogButton, NotificationLevel, ReviewDialogFocus, ReviewDialogState, ViewMode,
    };
    use crate::input::mouse::handle_mouse_event;

    /// Builds a review dialog whose directory section derives from the given
    /// repository file list.
    fn review_dialog_with_files(files: &[&str]) -> ReviewDialogState {
        ReviewDialogState::new(
            0,
            0,
            // One configured reviewer keeps the selector live, which is the
            // production shape: an empty list early-returns from
            // `ModelSelector::handle_key` and cannot observe the arrow routing
            // (c-3, c-4, c-5).
            vec![tauqe_protocol::ModelEntry::new(
                tauqe_protocol::ModelRef::openrouter("reviewer-model"),
                None,
            )],
            files.iter().map(|path| path.to_string()).collect(),
        )
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// The Confirm click runs the same launch helper as the keyboard, so a
    /// target pattern that selects no repository file is refused: the warning
    /// names the pattern, the dialog keeps its fields, and the transport stays
    /// silent (c-2, c-4).
    #[tokio::test]
    async fn confirm_click_refuses_a_pattern_without_matches() {
        let term = Rect::new(0, 0, 120, 50);
        let mut dialog = review_dialog_with_files(&["src/lib.rs"]);
        dialog.path_inputs[0].insert_str("srx/*.rs");
        dialog.focus = ReviewDialogFocus::Actions;
        dialog.confirm_button = ConfirmDialogButton::Confirm;

        let geo = crate::dialogs::render::review_dialog_geometry(term, &dialog);
        let mouse = left_click(geo.confirm_button.x + 1, geo.confirm_button.y);

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.modals.review_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_modal_click(mouse, term, &state, guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        assert!(
            conn.written().is_empty(),
            "a refused launch never reaches the daemon"
        );

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .review_dialog
            .as_ref()
            .expect("the refused launch keeps the dialog open");
        assert_eq!(dialog.path_inputs[0].get_text(), "srx/*.rs");
        let notification = guard
            .active_notification()
            .expect("the refusal explains itself");
        assert_eq!(notification.level, NotificationLevel::Warning);
        assert!(
            notification.text.contains("srx/*.rs"),
            "the warning names the offending pattern: {}",
            notification.text
        );
    }

    /// A Confirm click with a pattern that selects files launches the review
    /// through the same typed request the keyboard sends (c-2, c-4).
    #[tokio::test]
    async fn confirm_click_requests_the_window_projection_for_a_matching_pattern() {
        let term = Rect::new(0, 0, 120, 50);
        let mut dialog = review_dialog_with_files(&["src/lib.rs"]);
        dialog.path_inputs[0].insert_str("*.rs");
        dialog.focus = ReviewDialogFocus::Actions;
        dialog.confirm_button = ConfirmDialogButton::Confirm;
        let geo = crate::dialogs::render::review_dialog_geometry(term, &dialog);
        let mouse = left_click(geo.confirm_button.x + 1, geo.confirm_button.y);

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.modals.review_dialog = Some(dialog);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_modal_click(mouse, term, &state, guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/preview"), "{payload}");
        assert!(payload.contains("path_patterns"), "{payload}");
        assert!(payload.contains("*.rs"), "{payload}");

        let guard = state.lock().await;
        assert!(
            guard.modals.review_dialog.is_none(),
            "a launchable dialog starts its window projection"
        );
        let pending = guard
            .modals
            .pending_review_launch
            .as_ref()
            .expect("the launch awaits its projection");
        assert_eq!(pending.params.path_patterns, vec!["*.rs".to_string()]);
    }
    /// A click on a preset row selects that row and hands the keyboard to the
    /// section, exactly like the reviewer and directory rows above it, and the
    /// row is translated through the selector's own scroll arithmetic (c-5).
    #[tokio::test]
    async fn preset_row_click_selects_it_and_focuses_the_section() {
        let term = Rect::new(0, 0, 120, 50);
        let dialog = review_dialog_with_files(&["src/lib.rs"]).with_presets(vec![
            tauqe_protocol::ReviewPresetSummary {
                name: "backend".to_string(),
                title: Some("Backend audit".to_string()),
            },
        ]);
        let geo = crate::dialogs::render::review_dialog_geometry(term, &dialog);
        let mouse = left_click(geo.preset_rows.x + 1, geo.preset_rows.y + 1);

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.modals.review_dialog = Some(dialog);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        let res = handle_modal_click(mouse, term, &state, guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");
        assert_eq!(res, InputResult::Continue);

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .review_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert_eq!(dialog.preset_selector.selected_index(), 1);
        assert_eq!(
            dialog.selected_preset().map(|preset| preset.name.as_str()),
            Some("backend")
        );
        assert_eq!(dialog.focus, ReviewDialogFocus::PresetList);
        drop(guard);
        assert!(
            conn.written().is_empty(),
            "selecting a preset starts nothing"
        );
    }
    /// A click on the post-analysis `Run Analysis` button dispatches the same
    /// typed request the keyboard confirmation sends (c-4).
    #[tokio::test]
    async fn review_analyze_confirm_click_submits_the_request() {
        let term = Rect::new(0, 0, 120, 40);
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.modals.confirm_review_analyze = Some(crate::app::ConfirmReviewAnalyzeState::new(
            "rev-1".to_string(),
            "Session rev-1".to_string(),
            vec![
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("architect-a"),
                    None,
                ),
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("architect-b"),
                    None,
                ),
            ],
        ));
        {
            let target = st
                .modals
                .confirm_review_analyze
                .as_mut()
                .expect("the dialog is open");
            target.selector.select_index(1);
        }
        let geom = crate::dialogs::render::review_analyze_dialog_geometry(term, 2);
        let mouse = left_click(geom.confirm_button.x + 1, geom.confirm_button.y);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        let res = handle_modal_click(mouse, term, &state, guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");
        assert_eq!(res, InputResult::Continue);

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        assert!(payload.contains("review/analyze"), "{payload}");
        assert!(
            payload.contains("architect-b"),
            "the selected architect must travel on the wire: {payload}"
        );

        let guard = state.lock().await;
        assert!(guard.modals.confirm_review_analyze.is_none());
        assert!(guard.review.analyzing);
    }

    /// A click on a completion row accepts that directory into the active path
    /// row, exactly as `Tab` accepts the highlighted candidate (c-5).
    #[tokio::test]
    async fn completion_row_click_fills_the_active_path_row() {
        let term = Rect::new(0, 0, 120, 50);
        let mut dialog = review_dialog_with_files(&["crates/core/src/lib.rs", "src/lib.rs"]);
        dialog.focus = ReviewDialogFocus::PathEditor;
        dialog.path_inputs[0].insert_str("cra");
        dialog.refresh_path_completion();
        let geo = crate::dialogs::render::review_dialog_geometry(term, &dialog);
        assert!(geo.completions.height > 0);
        let mouse = left_click(geo.completions.x + 2, geo.completions.y + 1);

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.modals.review_dialog = Some(dialog);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_modal_click(mouse, term, &state, guard, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .review_dialog
            .as_ref()
            .expect("the dialog stays open");
        assert_eq!(dialog.path_inputs[0].get_text(), "crates/");
        drop(guard);
        assert!(conn.written().is_empty());
    }
    /// The cache browser mirrors the keyboard: a click highlights the row it
    /// lands on, and a click on the highlighted directory folds it.
    #[tokio::test]
    async fn cache_browser_row_click_moves_the_highlight() {
        let term = Rect::new(0, 0, 120, 40);
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Context;
        st.last_frame_area = term;
        st.modals.outer_docs_cache = Some(crate::app::OuterDocsCacheDialogState {
            entries: vec![
                tauqe_protocol::OuterDocsCacheEntry {
                    path: "openrouter/models".to_string(),
                    size_bytes: 1024,
                    estimated_tokens: 256,
                },
                tauqe_protocol::OuterDocsCacheEntry {
                    path: "rust-book/ch04".to_string(),
                    size_bytes: 512,
                    estimated_tokens: 128,
                },
            ],
            ..Default::default()
        });

        let tree = crate::dialogs::render::outer_docs_cache_tree_rect(term);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let guard = state.lock().await;
        handle_modal_click(
            left_click(tree.x + 2, tree.y + 2),
            term,
            &state,
            guard,
            &mut conn,
        )
        .await
        .expect("in-memory dispatch never fails");

        assert!(conn.written().is_empty());
        let guard = state.lock().await;
        let dialog = guard
            .modals
            .outer_docs_cache
            .as_ref()
            .expect("the browser stays open");
        assert_eq!(dialog.cursor, 2);
    }
    /// A wheel notch delivered by the given kind at the given pointer position.
    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// A wheel notch that arrives on a surface without a scrollable body is
    /// consumed by that surface: it never leaks into the view underneath the
    /// card, so the review list keeps its reading position while its launcher
    /// owns the keyboard (c-1, Conventions §6).
    #[tokio::test]
    async fn wheel_on_a_non_scrollable_modal_never_moves_the_view_behind_it() {
        let term = Rect::new(0, 0, 120, 50);
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = term;
        st.review.view_height = 20;
        st.review.rendered_lines = 500;
        st.review.scroll = 0;
        st.modals.review_dialog = Some(review_dialog_with_files(&["src/lib.rs"]));

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        for _ in 0..2 {
            handle_mouse_event(wheel(MouseEventKind::ScrollDown, 40, 20), &state, &mut conn)
                .await
                .expect("in-memory dispatch never fails");
        }

        let guard = state.lock().await;
        assert_eq!(
            guard.review.scroll, 0,
            "the list behind the dialog must keep its reading position"
        );
        assert!(conn.written().is_empty());
    }

    /// A pending cache removal outranks the browser it was raised from, so the
    /// wheel is consumed by the decision and the tree behind it keeps both its
    /// highlight and its offset (Conventions §5.4, §6).
    #[tokio::test]
    async fn wheel_on_a_pending_cache_removal_never_moves_the_tree() {
        let term = Rect::new(0, 0, 120, 40);
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Context;
        st.last_frame_area = term;
        st.modals.outer_docs_cache = Some(crate::app::OuterDocsCacheDialogState {
            entries: vec![
                tauqe_protocol::OuterDocsCacheEntry {
                    path: "openrouter/models".to_string(),
                    size_bytes: 1024,
                    estimated_tokens: 256,
                },
                tauqe_protocol::OuterDocsCacheEntry {
                    path: "rust-book/ch04".to_string(),
                    size_bytes: 512,
                    estimated_tokens: 128,
                },
            ],
            ..Default::default()
        });
        st.modals.confirm_outer_docs_cache_remove = Some("openrouter/models".to_string());

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        for kind in [MouseEventKind::ScrollDown, MouseEventKind::ScrollUp] {
            handle_mouse_event(wheel(kind, 40, 20), &state, &mut conn)
                .await
                .expect("in-memory dispatch never fails");
        }

        let guard = state.lock().await;
        let dialog = guard
            .modals
            .outer_docs_cache
            .as_ref()
            .expect("the browser stays open behind the card");
        assert_eq!(dialog.cursor, 0, "the pending decision owns the wheel");
        assert_eq!(dialog.scroll, 0);
        assert!(conn.written().is_empty());
    }

    /// The inline Context path input sits outside the modal stack and keeps the
    /// wheel for itself, so typing a path never scrolls the list underneath it
    /// (Conventions §5.1).
    #[tokio::test]
    async fn wheel_is_consumed_while_the_inline_context_input_is_open() {
        let term = Rect::new(0, 0, 100, 30);
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Context;
        st.last_frame_area = term;
        st.context_view.adding_file = true;
        st.context_view.cursor_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(wheel(MouseEventKind::ScrollDown, 40, 20), &state, &mut conn)
            .await
            .expect("in-memory dispatch never fails");

        let guard = state.lock().await;
        assert_eq!(
            guard.context_view.cursor_index, 1,
            "the picker list keeps its row while a path is typed"
        );
    }
}
