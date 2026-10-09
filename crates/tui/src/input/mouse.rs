use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::methods;

use crate::app::{AppState, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

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

pub fn check_confirm_hit(geom: &crate::ui::dialogs::ConfirmModalGeometry, col: u16, row: u16) -> ModalHit {
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

pub async fn handle_mouse_event(
    mouse: MouseEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(ref mut dialog) = st.status_dialog {
                dialog.selected_index = dialog.selected_index.saturating_sub(1);
                return Ok(InputResult::Continue);
            }
            if let Some(ref mut dialog) = st.selection_dialog {
                let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                let area = crate::ui::selection_dialog_area(Rect::new(0, 0, term_w, term_h), dialog.items.len());
                let visible_height = crate::ui::dialogs::selection_dialog_visible_height(area);
                dialog.select_prev(visible_height);
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            // Allow wheel scrolling in squash dialog (diff or file list)
            if let Some(ref mut dialog) = st.squash_dialog {
                if dialog.confirm.is_none() {
                    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                    let term_rect = Rect::new(0, 0, term_w, term_h);
                    let (_area, files_rect, _diff_rect) = crate::ui::dialogs::squash_dialog_chunks(term_rect);
                    if rect_contains(files_rect, mouse.column, mouse.row) {
                        if dialog.selected_file_index > 0 {
                            dialog.selected_file_index -= 1;
                            dialog.diff_scroll = 0;
                        }
                    } else {
                        dialog.diff_scroll = dialog.diff_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                    }
                    return Ok(InputResult::Continue);
                }
            }
            // Do not scroll background content when modal dialogs are open
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }
            match st.view_mode {
                ViewMode::Develop => {
                    st.model.auto_scroll = false;
                    st.model.scroll = st.model.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::History => {
                    st.history_view.auto_scroll = false;
                    st.history_view.scroll = st.history_view.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                    let h = st.last_model_height;
                    let w = st.history_view.content_width;
                    st.history_view.update_selected_item_from_scroll(h, Some(w));
                }
                ViewMode::Context => {
                    st.context_view.cursor_index =
                        st.context_view.cursor_index.saturating_sub(MOUSE_SCROLL_STEP);
                }
                ViewMode::Review => {
                    st.review.scroll = st.review.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::Plans => {
                    st.plans_view.scroll = st.plans_view.scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                }
                ViewMode::Onboarding => {}
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::ScrollDown => {
            if let Some(ref mut dialog) = st.status_dialog {
                let total = match &dialog.target {
                    crate::app::StatusDialogTarget::PlanItem { .. } => 4,
                    crate::app::StatusDialogTarget::ReviewItem { .. } => 3,
                };
                if dialog.selected_index + 1 < total {
                    dialog.selected_index += 1;
                }
                return Ok(InputResult::Continue);
            }
            if let Some(ref mut dialog) = st.selection_dialog {
                let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                let area = crate::ui::selection_dialog_area(Rect::new(0, 0, term_w, term_h), dialog.items.len());
                let visible_height = crate::ui::dialogs::selection_dialog_visible_height(area);
                dialog.select_next(visible_height);
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
            }
            // Allow wheel scrolling in squash dialog (diff or file list)
            if let Some(ref mut dialog) = st.squash_dialog {
                if dialog.confirm.is_none() {
                    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                    let term_rect = Rect::new(0, 0, term_w, term_h);
                    let (_area, files_rect, _diff_rect) = crate::ui::dialogs::squash_dialog_chunks(term_rect);
                    if rect_contains(files_rect, mouse.column, mouse.row) {
                        if !dialog.files.is_empty() && dialog.selected_file_index + 1 < dialog.files.len() {
                            dialog.selected_file_index += 1;
                            dialog.diff_scroll = 0;
                        }
                    } else {
                        dialog.diff_scroll = dialog.diff_scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                    }
                    return Ok(InputResult::Continue);
                }
            }
            // Do not scroll background content when modal dialogs are open
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }
            match st.view_mode {
                ViewMode::Develop => {
                    let view_height = st.last_model_height;
                    let max = st.model.max_scroll(view_height);
                    st.model.scroll = (st.model.scroll.saturating_add(MOUSE_SCROLL_STEP as u16)).min(max);
                    if st.model.scroll >= max {
                        st.model.auto_scroll = true;
                    }
                }
                ViewMode::History => {
                    let view_height = st.last_model_height;
                    let total = st.history_view.rendered_lines_count as u16;
                    let max = total.saturating_sub(view_height);
                    st.history_view.scroll = (st.history_view.scroll.saturating_add(MOUSE_SCROLL_STEP as u16)).min(max);
                    let w = st.history_view.content_width;
                    st.history_view.update_selected_item_from_scroll(view_height, Some(w));
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
                    let max = (st.review.rendered_lines as u16).saturating_sub(st.review.view_height);
                    st.review.scroll = st.review.scroll.saturating_add(MOUSE_SCROLL_STEP as u16).min(max);
                }
                ViewMode::Plans => {
                    let max = (st.plans_view.rendered_lines as u16).saturating_sub(st.plans_view.view_height);
                    st.plans_view.scroll = st.plans_view.scroll.saturating_add(MOUSE_SCROLL_STEP as u16).min(max);
                }
                ViewMode::Onboarding => {}
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(dialog) = st.selection_dialog.as_ref() {
                let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                let item_count = dialog.items.len();
                let area = crate::ui::selection_dialog_area(Rect::new(0, 0, term_w, term_h), item_count);

                if mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    let inner_height = crate::ui::dialogs::selection_dialog_visible_height(area);
                    let max_scroll = dialog.items.len().saturating_sub(inner_height);
                    let scroll_offset = dialog.scroll_offset.min(max_scroll);
                    let start_y = area.y + 2;
                    if mouse.row >= start_y && (mouse.row as usize) < start_y as usize + inner_height {
                        let clicked_visible = (mouse.row - start_y) as usize;
                        let clicked_idx = scroll_offset + clicked_visible;
                        if let Some(chosen) = dialog.items.get(clicked_idx).cloned() {
                            crate::input::dialogs::apply_model_selection(st, chosen, server_writer)
                                .await?;
                            return Ok(InputResult::Continue);
                        }
                    } else {
                        let footer_y = area.y + area.height.saturating_sub(3);
                        if mouse.row >= footer_y {
                            st.selection_dialog = None;
                        }
                    }
                    return Ok(InputResult::Continue);
                } else {
                    st.selection_dialog = None;
                    return Ok(InputResult::Continue);
                }
            }

            if st.show_help {
                st.show_help = false;
                st.help_scroll = 0;
                return Ok(InputResult::Continue);
            }

            let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
            let term_rect = Rect::new(0, 0, term_w, term_h);

            // 0. Status dialog hit-testing
            if let Some(dialog) = st.status_dialog.clone() {
                let options_count = match &dialog.target {
                    crate::app::StatusDialogTarget::PlanItem { .. } => 4,
                    crate::app::StatusDialogTarget::ReviewItem { .. } => 3,
                };
                let area = crate::ui::dialogs::status_dialog_area(term_rect, options_count);
                if rect_contains(area, mouse.column, mouse.row) {
                    let start_y = area.y + 4; // header border (1) + top margin (1) + target preview (2)
                    let clicked_row = mouse.row.saturating_sub(start_y) as usize;
                    if clicked_row < options_count {
                        crate::input::dialogs::apply_status_dialog(&mut st, clicked_row, server_writer).await?;
                    } else {
                        let footer_y = area.y + area.height.saturating_sub(3);
                        if mouse.row >= footer_y {
                            st.status_dialog = None;
                        }
                    }
                } else {
                    st.status_dialog = None;
                }
                return Ok(InputResult::Continue);
            }

            // 1. Server disconnected popup button hit-testing
            if st.server_disconnected.is_some() {
                let area = crate::ui::centered_rect(64, 30, term_rect);
                let inner_y = area.y + 1;
                let inner_h = area.height.saturating_sub(2);
                let inner_x = area.x + 1;
                let inner_w = area.width.saturating_sub(2);
                let btn_y = inner_y + inner_h.saturating_sub(3);
                let btn_len = " [ Quit TAUQE (q / Ctrl+Q) ] ".len() as u16;
                let start_x = inner_x + (inner_w.saturating_sub(btn_len)) / 2;
                let quit_btn = Rect::new(start_x, btn_y, btn_len, 1);
                if rect_contains(quit_btn, mouse.column, mouse.row) || !rect_contains(area, mouse.column, mouse.row) {
                    return Ok(InputResult::Exit);
                }
                return Ok(InputResult::Continue);
            }

            // 2. Squash dialog confirm modal & browsing hit-testing
            if let Some(ref mut dialog) = st.squash_dialog {
                if let Some(kind) = dialog.confirm {
                    let (w, lines_len, c_label, d_label) = match kind {
                        crate::app::SquashConfirm::Apply => (60, 6, "Confirm Squash (Y)", "Cancel (Esc)"),
                        crate::app::SquashConfirm::Discard => (58, 4, "Discard (Y)", "Cancel (Esc)"),
                    };
                    let geom = crate::ui::dialogs::confirm_modal_geometry(term_rect, w, lines_len, c_label, d_label);
                    match check_confirm_hit(&geom, mouse.column, mouse.row) {
                        ModalHit::Confirm => {
                            dialog.confirm = None;
                            match kind {
                                crate::app::SquashConfirm::Apply => {
                                    let base_ref = dialog.base_ref.clone();
                                    let message = dialog.message_editor.get_text().trim().to_string();
                                    dialog.applying = true;
                                    dialog.status_message = Some("Applying squash...".to_string());
                                    drop(st);
                                    let params = tauqe_protocol::GitSquashApplyParams { base_ref, message };
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

                let (area, files_rect, _diff_rect) = crate::ui::dialogs::squash_dialog_chunks(term_rect);
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
                    let files_offset = (dialog.selected_file_index + 1).saturating_sub(files_visible);
                    let clicked_row = (mouse.row.saturating_sub(files_rect.y + 1)) as usize;
                    let clicked_idx = files_offset + clicked_row;
                    if clicked_idx < dialog.files.len() {
                        if clicked_idx == dialog.selected_file_index {
                            dialog.files[clicked_idx].expanded = !dialog.files[clicked_idx].expanded;
                        } else {
                            dialog.selected_file_index = clicked_idx;
                            dialog.diff_scroll = 0;
                        }
                        dialog.focus = crate::app::SquashDialogFocus::FileList;
                    }
                    return Ok(InputResult::Continue);
                }
                return Ok(InputResult::Continue);
            }

            // 2b. Discuss plan dialog hit-testing
            if let Some(dialog) = st.discuss_plan_dialog.clone() {
                let area = crate::ui::centered_rect(68, 56, term_rect);
                let inner_y = area.y + 1;
                let inner_h = area.height.saturating_sub(2);
                let inner_x = area.x + 1;
                let inner_w = area.width.saturating_sub(2);
                let btn_y = inner_y + inner_h.saturating_sub(3);
                let start_x = inner_x + (inner_w.saturating_sub(52)) / 2;
                let confirm_btn = Rect::new(start_x, btn_y, 30, 1);
                let cancel_btn = Rect::new(start_x + 34, btn_y, 18, 1);

                if rect_contains(confirm_btn, mouse.column, mouse.row) {
                    st.discuss_plan_dialog = None;
                    let user_comment = dialog.prompt_editor.get_text().trim().to_string();
                    if !user_comment.is_empty() {
                        if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == dialog.plan_id).cloned() {
                            let focused_ids: Vec<String> = dialog.focused_items.iter().map(|(id, _)| id.clone()).collect();
                            let prompt = tauqe_protocol::plan::format_plan_discussion_prompt(
                                &plan,
                                &focused_ids,
                                &user_comment,
                            );
                            st.view_mode = ViewMode::Develop;
                            st.model.reasoning.clear();
                            st.model.text.clear();
                            st.model.markdown_lines.clear();
                            st.model.error = None;
                            st.model.result = None;
                            st.model.usage = None;
                            st.model.scroll = 0;
                            st.model.status = "awaiting".to_string();
                            st.model.auto_scroll = true;
                            st.model.current_cost = Some(0.0);
                            st.model.edits_active = false;
                            st.model.files.clear();
                            st.model.selected_file_index = 0;
                            st.model.edit_final_applied = None;
                            st.model.edit_final_error = None;
                            st.model.last_commit_hash = None;
                            st.model.last_commit_summary = None;
                            st.model.toolchain_command = None;
                            st.model.toolchain_status = None;
                            st.model.copy_flash = None;
                            st.model.code_blocks.clear();
                            st.turn_started_at = Some(std::time::Instant::now());

                            drop(st);
                            send_request(
                                server_writer,
                                methods::MODEL_ASK,
                                serde_json::json!({ "prompt": prompt }),
                            )
                            .await?;
                            return Ok(InputResult::Continue);
                        }
                    }
                } else if rect_contains(cancel_btn, mouse.column, mouse.row) || !rect_contains(area, mouse.column, mouse.row) {
                    st.discuss_plan_dialog = None;
                    return Ok(InputResult::Continue);
                } else {
                    return Ok(InputResult::Continue);
                }
            }

            // 3. Review launch dialog hit-testing
            if let Some(dialog) = st.review_dialog.clone() {
                let area = crate::ui::centered_rect(64, 54, term_rect);
                let inner_y = area.y + 1;
                let inner_h = area.height.saturating_sub(2);
                let inner_x = area.x + 1;
                let inner_w = area.width.saturating_sub(2);
                let btn_y = inner_y + inner_h.saturating_sub(3);
                let start_x = inner_x + (inner_w.saturating_sub(48)) / 2;
                let confirm_btn = Rect::new(start_x, btn_y, 26, 1);
                let cancel_btn = Rect::new(start_x + 30, btn_y, 18, 1);

                if rect_contains(confirm_btn, mouse.column, mouse.row) {
                    st.review_dialog = None;
                    let prompt = dialog.prompt_editor.get_text().trim().to_string();
                    let params = tauqe_protocol::ReviewStartParams {
                        model: dialog.models.get(dialog.model_index).cloned(),
                        user_prompt: if prompt.is_empty() { None } else { Some(prompt) },
                    };
                    st.review.begin();
                    drop(st);
                    send_request(
                        server_writer,
                        methods::REVIEW_START,
                        serde_json::to_value(params)?,
                    )
                    .await?;
                    return Ok(InputResult::Continue);
                } else if rect_contains(cancel_btn, mouse.column, mouse.row) || !rect_contains(area, mouse.column, mouse.row) {
                    st.review_dialog = None;
                    return Ok(InputResult::Continue);
                } else {
                    return Ok(InputResult::Continue);
                }
            }

            // 4. Confirmation modals hit-testing (Cancel, Quit, Undo, Delete Plan, Clear Auto, Clear History)
            if st.confirm_cancel {
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, 5, "Interrupt (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_cancel = false;
                        let cancel_review = st.review.running;
                        let cancel_model = st.model.is_busy();
                        drop(st);
                        if cancel_review {
                            send_request(server_writer, methods::REVIEW_CANCEL, serde_json::json!({})).await?;
                        }
                        if cancel_model || !cancel_review {
                            send_request(server_writer, methods::MODEL_CANCEL, serde_json::json!({})).await?;
                        }
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_cancel = false;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if st.confirm_quit {
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, 5, "Quit TAUQE (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_quit = false;
                        return Ok(InputResult::Exit);
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_quit = false;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if st.confirm_undo {
                let lines_count = 5
                    + (st.model.last_commit_hash.is_some() as usize)
                    + (st.model.last_commit_summary.is_some() as usize);
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, lines_count, "Confirm Undo (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_undo = false;
                        drop(st);
                        send_request(server_writer, methods::GIT_UNDO, serde_json::json!({})).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_undo = false;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if let Some(target) = st.confirm_execute_scope.clone() {
                let is_single = target.steps.len() == 1;
                let c_label = if is_single {
                    "Execute (Enter)"
                } else {
                    "Execute Scope (Enter)"
                };
                let body_count = if is_single { 7 } else { 8 + target.steps.len().min(4) };
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 66, body_count, c_label, "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_execute_scope = None;
                        let first_step = match target.steps.first() {
                            Some(s) => s.clone(),
                            None => return Ok(InputResult::Continue),
                        };

                        if target.steps.len() > 1 {
                            st.plan_batch_queue = Some(crate::app::PlanBatchQueue {
                                plan_id: target.plan_id.clone(),
                                plan_title: target.plan_title.clone(),
                                scope_title: target.scope_title.clone(),
                                steps: target.steps.clone(),
                                current_index: 0,
                            });
                        } else {
                            st.plan_batch_queue = None;
                        }

                        if let Some(plan) = st.plans_view.plans.iter_mut().find(|p| p.id == target.plan_id) {
                            plan.update_item_status(&first_step.id, tauqe_protocol::PlanItemStatus::InProgress);
                        }
                        st.view_mode = ViewMode::Develop;
                        st.model.reasoning.clear();
                        st.model.text.clear();
                        st.model.markdown_lines.clear();
                        st.model.error = None;
                        st.model.result = None;
                        st.model.usage = None;
                        st.model.scroll = 0;
                        st.model.status = "awaiting".to_string();
                        st.model.auto_scroll = true;
                        st.model.current_cost = Some(0.0);
                        st.model.edits_active = false;
                        st.model.files.clear();
                        st.model.selected_file_index = 0;
                        st.model.edit_final_applied = None;
                        st.model.edit_final_error = None;
                        st.model.last_commit_hash = None;
                        st.model.last_commit_summary = None;
                        st.model.toolchain_command = None;
                        st.model.toolchain_status = None;
                        st.model.copy_flash = None;
                        st.model.code_blocks.clear();
                        st.turn_started_at = Some(std::time::Instant::now());

                        drop(st);
                        let params = tauqe_protocol::PlanExecuteStepParams {
                            plan_id: target.plan_id,
                            step_id: first_step.id,
                        };
                        send_request(
                            server_writer,
                            methods::PLAN_EXECUTE_STEP,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_execute_scope = None;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if let Some(plan_id) = st.confirm_delete_plan.clone() {
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, 5, "Confirm Delete (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_delete_plan = None;
                        drop(st);
                        crate::input::dialogs::delete_plan(server_writer, plan_id).await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_delete_plan = None;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if st.context_view.confirm_clear_auto {
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, 5, "Confirm Clear (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.context_view.confirm_clear_auto = false;
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
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            if st.confirm_clear_history {
                let geom = crate::ui::dialogs::confirm_modal_geometry(
                    term_rect, 58, 6, "Confirm Clear (Y)", "Cancel (Esc)",
                );
                match check_confirm_hit(&geom, mouse.column, mouse.row) {
                    ModalHit::Confirm => {
                        st.confirm_clear_history = false;
                        let cost = st.model.session_total_cost;
                        let prev_cost = st.model.prev_cost;
                        st.model = crate::ui::develop::DevelopView {
                            session_total_cost: cost,
                            prev_cost,
                            ..Default::default()
                        };
                        st.history_view = crate::ui::history::HistoryViewState::default();
                        drop(st);
                        send_request(
                            server_writer,
                            methods::MODEL_CLEAR_HISTORY,
                            serde_json::json!({}),
                        )
                        .await?;
                    }
                    ModalHit::Cancel | ModalHit::Outside => {
                        st.confirm_clear_history = false;
                    }
                    ModalHit::InsideBody => {}
                }
                return Ok(InputResult::Continue);
            }

            // If any other modal is active, swallow clicks to protect background state and tabs
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }

            if st.header_clicks.row > 0 && mouse.row == st.header_clicks.row {
                let areas = st.header_clicks;
                if areas.develop_tab.1 > 0 && mouse.column >= areas.develop_tab.0 && mouse.column <= areas.develop_tab.1 {
                    crate::input::switch_view(&mut st, ViewMode::Develop, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.context_tab.1 > 0 && mouse.column >= areas.context_tab.0 && mouse.column <= areas.context_tab.1 {
                    crate::input::switch_view(&mut st, ViewMode::Context, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.review_tab.1 > 0 && mouse.column >= areas.review_tab.0 && mouse.column <= areas.review_tab.1 {
                    crate::input::switch_view(&mut st, ViewMode::Review, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.plans_tab.1 > 0 && mouse.column >= areas.plans_tab.0 && mouse.column <= areas.plans_tab.1 {
                    crate::input::switch_view(&mut st, ViewMode::Plans, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.history_tab.1 > 0 && mouse.column >= areas.history_tab.0 && mouse.column <= areas.history_tab.1 {
                    crate::input::switch_view(&mut st, ViewMode::History, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.model_select.1 > 0 && mouse.column >= areas.model_select.0 && mouse.column <= areas.model_select.1 {
                    if st.model.is_busy() {
                        st.notify_warning("Cannot change model while model is generating");
                        return Ok(InputResult::Continue);
                    }
                    if !st.available_models.is_empty() {
                        let cur_idx = st
                            .available_models
                            .iter()
                            .position(|m| m == &st.active_model)
                            .unwrap_or(0);
                        st.selection_dialog = Some(crate::app::SelectionDialogState::new(
                            crate::app::SelectionDialogKind::Model,
                            st.available_models.clone(),
                            cur_idx,
                        ));
                        return Ok(InputResult::Continue);
                    }
                } else if areas.squash_button.1 > 0 && mouse.column >= areas.squash_button.0 && mouse.column <= areas.squash_button.1 {
                    crate::input::open_squash_dialog(&mut st, server_writer).await?;
                    return Ok(InputResult::Continue);
                } else if areas.help_button.1 > 0 && mouse.column >= areas.help_button.0 && mouse.column <= areas.help_button.1 {
                    st.show_help = !st.show_help;
                    return Ok(InputResult::Continue);
                }
            }

            if st.view_mode == ViewMode::Develop {
                let (cx, cy, cw, ch) = st.model.content_rect;
                if mouse.column >= cx && mouse.column < cx + cw && mouse.row >= cy && mouse.row < cy + ch {
                    let relative_row = (mouse.row - cy) as usize;
                    let clicked_visual_line = st.model.scroll as usize + relative_row;

                    if let Some(block) = st.model.code_blocks.iter().find(|b| {
                        clicked_visual_line >= b.visual_start_line
                            && clicked_visual_line <= b.visual_end_line
                    }).cloned() {
                        let lang_len = if block.lang.trim().is_empty() { 4 } else { block.lang.trim().chars().count() };
                        let btn_start = cx + 2 + lang_len as u16 + 1;
                        let btn_end = btn_start + 14;
                        let is_button_clicked = clicked_visual_line == block.visual_start_line
                            && mouse.column >= btn_start
                            && mouse.column <= btn_end;

                        if is_button_clicked {
                            let code_to_copy = block.code.clone();
                            let block_id = block.id;
                            match crate::clipboard::copy_to_clipboard(&code_to_copy) {
                                crate::clipboard::CopyResult::Native => {
                                    st.model.copy_flash = Some((block_id, std::time::Instant::now()));
                                    let lines_count = code_to_copy.lines().count().max(1);
                                    st.notify_success(format!("Copied {} lines of code to clipboard", lines_count));
                                    st.model.update_markdown();
                                }
                                crate::clipboard::CopyResult::Osc52Only => {
                                    st.model.copy_flash = Some((block_id, std::time::Instant::now()));
                                    let lines_count = code_to_copy.lines().count().max(1);
                                    st.notify_success(format!("Copied {} lines via terminal (OSC 52)", lines_count));
                                    st.model.update_markdown();
                                }
                                crate::clipboard::CopyResult::Failed => {
                                    st.notify_error("Failed to copy code to clipboard");
                                }
                            }
                        } else if !st.shift_tip_shown {
                            st.notify_info("Tip: Hold Shift while dragging to select text with mouse");
                            st.shift_tip_shown = true;
                        }
                    }
                }
            } else if st.view_mode == ViewMode::History {
                let (cx, cy, cw, ch) = st.history_view.content_rect;
                if mouse.column >= cx && mouse.column < cx + cw && mouse.row >= cy && mouse.row < cy + ch {
                    let relative_row = (mouse.row - cy) as usize;
                    let clicked_visual_line = st.history_view.scroll as usize + relative_row;

                    if let Some(block) = st.history_view.code_blocks.iter().find(|b| {
                        clicked_visual_line >= b.visual_start_line
                            && clicked_visual_line <= b.visual_end_line
                    }).cloned() {
                        let lang_len = if block.lang.trim().is_empty() { 4 } else { block.lang.trim().chars().count() };
                        let btn_start = cx + 2 + 2 + lang_len as u16 + 1;
                        let btn_end = btn_start + 14;
                        let is_button_clicked = clicked_visual_line == block.visual_start_line
                            && mouse.column >= btn_start
                            && mouse.column <= btn_end;

                        if is_button_clicked {
                            let code_to_copy = block.code.clone();
                            match crate::clipboard::copy_to_clipboard(&code_to_copy) {
                                crate::clipboard::CopyResult::Native => {
                                    st.history_view.copy_flash =
                                        Some(((block.item_id, block.block_id), std::time::Instant::now()));
                                    let lines_count = code_to_copy.lines().count().max(1);
                                    st.notify_success(format!("Copied {} lines of code to clipboard", lines_count));
                                }
                                crate::clipboard::CopyResult::Osc52Only => {
                                    st.history_view.copy_flash =
                                        Some(((block.item_id, block.block_id), std::time::Instant::now()));
                                    let lines_count = code_to_copy.lines().count().max(1);
                                    st.notify_success(format!("Copied {} lines via terminal (OSC 52)", lines_count));
                                }
                                crate::clipboard::CopyResult::Failed => {
                                    st.notify_error("Failed to copy code to clipboard");
                                }
                            }
                        } else if !st.shift_tip_shown {
                            st.notify_info("Tip: Hold Shift while dragging to select text with mouse");
                            st.shift_tip_shown = true;
                        }
                    }
                }
            }
            Ok(InputResult::Continue)
        }
        _ => Ok(InputResult::Continue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn test_rect_contains_and_check_confirm_hit() {
        let term = Rect::new(0, 0, 100, 30);
        let geom = crate::ui::dialogs::confirm_modal_geometry(term, 58, 5, "Interrupt (Y)", "Cancel (Esc)");

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
}
