use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tauqe_protocol::{methods, ConfigSetParams, GitSquashPreviewParams};

use crate::app::{AppState, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

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
            match st.view_mode {
                ViewMode::Develop => {
                    st.model.auto_scroll = false;
                    st.model.scroll = st.model.scroll.saturating_sub(3);
                }
                ViewMode::History => {
                    st.history_view.auto_scroll = false;
                    st.history_view.scroll = st.history_view.scroll.saturating_sub(3);
                }
                ViewMode::Context => {
                    if st.context_view.cursor_index > 0 {
                        st.context_view.cursor_index =
                            st.context_view.cursor_index.saturating_sub(1);
                    }
                }
                ViewMode::Review => {
                    st.review.scroll = st.review.scroll.saturating_sub(3);
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
            match st.view_mode {
                ViewMode::Develop => {
                    let view_height = st.last_model_height;
                    let max = st.model.max_scroll(view_height);
                    st.model.scroll = (st.model.scroll.saturating_add(3)).min(max);
                    if st.model.scroll >= max {
                        st.model.auto_scroll = true;
                    }
                }
                ViewMode::History => {
                    let view_height = st.last_model_height;
                    let total = st.history_view.rendered_lines_count as u16;
                    let max = total.saturating_sub(view_height);
                    st.history_view.scroll = (st.history_view.scroll.saturating_add(3)).min(max);
                    if st.history_view.scroll >= max {
                        st.history_view.auto_scroll = true;
                    }
                }
                ViewMode::Context => {
                    let rows_len = st.context_view.compute_rows(&st.context.items).len();
                    if rows_len > 0 && st.context_view.cursor_index + 1 < rows_len {
                        st.context_view.cursor_index += 1;
                    }
                }
                ViewMode::Review => {
                    let max = (st.review.rendered_lines as u16).saturating_sub(st.review.view_height);
                    st.review.scroll = st.review.scroll.saturating_add(3).min(max);
                }
                ViewMode::Onboarding => {}
            }
            Ok(InputResult::Continue)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(dialog) = st.selection_dialog.as_ref() {
                let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
                let term_area = Rect::new(0, 0, term_w, term_h);
                let item_count = dialog.items.len();
                let height = (item_count as u16 + 4).clamp(5, 16);
                let area = crate::ui::centered_rect(
                    50,
                    (height * 100 / term_area.height.max(1)).clamp(15, 60),
                    term_area,
                );

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
                            st.active_model = chosen.clone();
                            st.selection_dialog = None;
                            let params = ConfigSetParams {
                                workflow: None,
                                edit_protocol: None,
                                model: Some(chosen),
                            };
                            drop(st);
                            send_request(
                                server_writer,
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

            if mouse.row == 1 {
                let areas = st.header_clicks;
                if mouse.column >= areas.develop_tab.0 && mouse.column <= areas.develop_tab.1 {
                    st.view_mode = ViewMode::Develop;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if mouse.column >= areas.context_tab.0 && mouse.column <= areas.context_tab.1 {
                    st.view_mode = ViewMode::Context;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if mouse.column >= areas.review_tab.0 && mouse.column <= areas.review_tab.1 {
                    st.view_mode = ViewMode::Review;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if mouse.column >= areas.history_tab.0 && mouse.column <= areas.history_tab.1 {
                    st.view_mode = ViewMode::History;
                    st.history_view.auto_scroll = true;
                    st.context_view.status_message = None;
                    return Ok(InputResult::Continue);
                } else if mouse.column >= areas.model_select.0 && mouse.column <= areas.model_select.1 {
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
                } else if mouse.column >= areas.squash_button.0 && mouse.column <= areas.squash_button.1 {
                    if st.model.is_busy() {
                        st.model.git_notification = Some(
                            "Cannot squash commits while model is generating".to_string(),
                        );
                    } else {
                        st.squash_dialog = Some(crate::app::SquashDialogState::default());
                        drop(st);
                        let params = GitSquashPreviewParams { base_ref: None };
                        send_request(
                            server_writer,
                            methods::GIT_SQUASH_PREVIEW,
                            serde_json::to_value(params)?,
                        )
                        .await?;
                        return Ok(InputResult::Continue);
                    }
                } else if mouse.column >= areas.help_button.0 && mouse.column <= areas.help_button.1 {
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
                        let code_to_copy = block.code.clone();
                        let block_id = block.id;
                        crate::clipboard::copy_to_clipboard(&code_to_copy);
                        st.model.copy_flash = Some((block_id, std::time::Instant::now()));
                        let lines_count = code_to_copy.lines().count().max(1);
                        st.model.copy_notification = Some((
                            format!("Copied {} lines to clipboard", lines_count),
                            std::time::Instant::now(),
                        ));
                        st.model.update_markdown();
                    }
                }
            }
            Ok(InputResult::Continue)
        }
        _ => Ok(InputResult::Continue),
    }
}
