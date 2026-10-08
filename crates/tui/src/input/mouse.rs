use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ConfigSetParams};

use crate::app::{AppState, ViewMode};
use crate::input::InputResult;
use crate::rpc::{allocate_request_id, record_optimistic_rollback, send_request, send_request_with_id, OptimisticRollback};

pub const MOUSE_SCROLL_STEP: usize = 3;

pub async fn handle_mouse_event(
    mouse: MouseEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(ref mut dialog) = st.selection_dialog {
                dialog.selected_index = dialog.selected_index.saturating_sub(1);
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_sub(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
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
            if let Some(ref mut dialog) = st.selection_dialog {
                if !dialog.items.is_empty() && dialog.selected_index + 1 < dialog.items.len() {
                    dialog.selected_index += 1;
                }
                return Ok(InputResult::Continue);
            }
            if st.show_help {
                st.help_scroll = st.help_scroll.saturating_add(MOUSE_SCROLL_STEP as u16);
                return Ok(InputResult::Continue);
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
                    let start_y = area.y + 2;
                    if mouse.row >= start_y
                        && (mouse.row as usize) < start_y as usize + item_count
                    {
                        let clicked_idx = (mouse.row - start_y) as usize;
                        if let Some(chosen) = dialog.items.get(clicked_idx).cloned() {
                            let prev_model = st.active_model.clone();
                            st.active_model = chosen.clone();
                            st.selection_dialog = None;
                            let req_id = allocate_request_id();
                            record_optimistic_rollback(
                                req_id,
                                OptimisticRollback::ActiveModel { prev_model },
                            );
                            let params = ConfigSetParams {
                                model: Some(chosen),
                                ..Default::default()
                            };
                            drop(st);
                            send_request_with_id(
                                server_writer,
                                req_id,
                                methods::CONFIG_SET,
                                serde_json::to_value(params)?,
                            )
                            .await?;
                            return Ok(InputResult::Continue);
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

            // If any modal is active, swallow clicks to protect background state and tabs
            if st.has_active_modal() {
                return Ok(InputResult::Continue);
            }

            if st.header_clicks.row > 0 && mouse.row == st.header_clicks.row {
                let areas = st.header_clicks;
                if areas.develop_tab.1 > 0 && mouse.column >= areas.develop_tab.0 && mouse.column <= areas.develop_tab.1 {
                    st.view_mode = ViewMode::Develop;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if areas.context_tab.1 > 0 && mouse.column >= areas.context_tab.0 && mouse.column <= areas.context_tab.1 {
                    st.view_mode = ViewMode::Context;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if areas.review_tab.1 > 0 && mouse.column >= areas.review_tab.0 && mouse.column <= areas.review_tab.1 {
                    st.view_mode = ViewMode::Review;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if areas.plans_tab.1 > 0 && mouse.column >= areas.plans_tab.0 && mouse.column <= areas.plans_tab.1 {
                    st.view_mode = ViewMode::Plans;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if areas.history_tab.1 > 0 && mouse.column >= areas.history_tab.0 && mouse.column <= areas.history_tab.1 {
                    st.view_mode = ViewMode::History;
                    st.history_view.auto_scroll = true;
                    if !st.history_view.items.is_empty() {
                        st.history_view.selected_item_index =
                            st.history_view.items.len().saturating_sub(1);
                    } else if !st.history_view.loading {
                        st.history_view.loading = true;
                        drop(st);
                        let params = tauqe_protocol::HistoryGetParams {
                            limit: Some(20),
                            before_id: None,
                        };
                        send_request(
                            server_writer,
                            methods::HISTORY_GET,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                        return Ok(InputResult::Continue);
                    }
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if areas.model_select.1 > 0 && mouse.column >= areas.model_select.0 && mouse.column <= areas.model_select.1 {
                    if !st.model.is_busy() && !st.available_models.is_empty() {
                        let cur_idx = st
                            .available_models
                            .iter()
                            .position(|m| m == &st.active_model)
                            .unwrap_or(0);
                        st.selection_dialog = Some(crate::app::SelectionDialogState {
                            kind: crate::app::SelectionDialogKind::Model,
                            items: st.available_models.clone(),
                            selected_index: cur_idx,
                        });
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
                        } else {
                            st.notify_info("Tip: Hold Shift while dragging to select text with mouse");
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
                        } else {
                            st.notify_info("Tip: Hold Shift while dragging to select text with mouse");
                        }
                    }
                }
            }
            Ok(InputResult::Continue)
        }
        _ => Ok(InputResult::Continue),
    }
}
