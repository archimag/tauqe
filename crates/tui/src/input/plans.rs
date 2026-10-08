use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tauqe_protocol::{methods, PlanSetActiveParams, PlanUpdateItemParams};
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::input::InputResult;
use crate::rpc::send_request;

pub const PLANS_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "↑/↓ or k/j", description: "Select previous / next plan item" },
    KeyCommand { key: "Tab / Space", description: "Fold / unfold item details & subtasks" },
    KeyCommand { key: "x", description: "Check / uncheck item to focus in Develop context" },
    KeyCommand { key: "t", description: "Cycle status: Todo → InProgress → Done → Cancelled" },
    KeyCommand { key: "a", description: "Toggle fold / unfold all items" },
    KeyCommand { key: "←/→ or h/l", description: "Switch between multiple plans" },
    KeyCommand { key: "D / Delete", description: "Delete current plan (with confirmation)" },
    KeyCommand { key: "c / y", description: "Copy current plan as Markdown to clipboard" },
    KeyCommand { key: "r", description: "Refresh plans from server" },
    KeyCommand { key: "Esc", description: "Return to Develop view" },
];

async fn switch_plan(
    target_id: String,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<()> {
    send_request(
        server_writer,
        methods::PLAN_SET_ACTIVE,
        serde_json::to_value(PlanSetActiveParams { id: Some(target_id.clone()) })?,
    )
    .await?;
    send_request(
        server_writer,
        methods::PLAN_GET,
        serde_json::json!({ "id": target_id }),
    )
    .await?;
    Ok(())
}

pub async fn handle_plans_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    let rows_len = st.plans_view.flatten_items().len();
    let plans_len = st.plans_view.plans_list.len();

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if st.plans_view.selected_item_index > 0 {
                st.plans_view.selected_item_index -= 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if rows_len > 0 && st.plans_view.selected_item_index + 1 < rows_len {
                st.plans_view.selected_item_index += 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::Home => {
            st.plans_view.selected_item_index = 0;
            st.plans_view.scroll_to_selected();
        }
        KeyCode::End => {
            if rows_len > 0 {
                st.plans_view.selected_item_index = rows_len - 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::PageUp => {
            let h = st.plans_view.view_height;
            st.plans_view.scroll = st.plans_view.scroll.saturating_sub(h);
        }
        KeyCode::PageDown => {
            let h = st.plans_view.view_height;
            let max = (st.plans_view.rendered_lines as u16).saturating_sub(h);
            st.plans_view.scroll = st.plans_view.scroll.saturating_add(h).min(max);
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                if plans_len > 0 {
                    let prev_idx = if st.plans_view.selected_plan_index == 0 {
                        plans_len - 1
                    } else {
                        st.plans_view.selected_plan_index - 1
                    };
                    st.plans_view.selected_plan_index = prev_idx;
                    if let Some(target) = st.plans_view.plans_list.get(prev_idx).cloned() {
                        st.plans_view.active_plan_id = Some(target.id.clone());
                        st.plans_view.reset_view_for_new_plan();
                        drop(st);
                        switch_plan(target.id, server_writer).await?;
                        return Ok(InputResult::Continue);
                    }
                }
            } else {
                let sel_row = st.plans_view.selected_row();
                if let Some(row) = sel_row {
                    if row.is_expandable {
                        st.plans_view.toggle_expanded(&row.item_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                }
            }
        }
        KeyCode::Enter if st.plans_view.current_plan.is_none() => {
            if let Some(target) = st.plans_view.plans_list.get(st.plans_view.selected_plan_index).cloned() {
                st.plans_view.active_plan_id = Some(target.id.clone());
                st.plans_view.reset_view_for_new_plan();
                drop(st);
                switch_plan(target.id, server_writer).await?;
                return Ok(InputResult::Continue);
            }
        }
        KeyCode::Char(' ') | KeyCode::Enter => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                if row.is_expandable {
                    st.plans_view.toggle_expanded(&row.item_id);
                    st.plans_view.clamp_selection();
                    st.plans_view.scroll_to_selected();
                }
            }
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            if st.plans_view.expanded_items.is_empty() {
                st.plans_view.expand_all();
            } else {
                st.plans_view.collapse_all();
            }
            st.plans_view.clamp_selection();
            st.plans_view.scroll_to_selected();
        }
        KeyCode::BackTab => {
            if plans_len > 1 {
                let prev_idx = if st.plans_view.selected_plan_index == 0 {
                    plans_len - 1
                } else {
                    st.plans_view.selected_plan_index - 1
                };
                st.plans_view.selected_plan_index = prev_idx;
                if let Some(target) = st.plans_view.plans_list.get(prev_idx).cloned() {
                    st.plans_view.active_plan_id = Some(target.id.clone());
                    st.plans_view.reset_view_for_new_plan();
                    drop(st);
                    switch_plan(target.id, server_writer).await?;
                }
            } else {
                drop(st);
                send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
            }
        }
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
            if plans_len > 1 {
                let prev_idx = if st.plans_view.selected_plan_index == 0 {
                    plans_len - 1
                } else {
                    st.plans_view.selected_plan_index - 1
                };
                st.plans_view.selected_plan_index = prev_idx;
                if let Some(target) = st.plans_view.plans_list.get(prev_idx).cloned() {
                    st.plans_view.active_plan_id = Some(target.id.clone());
                    st.plans_view.reset_view_for_new_plan();
                    drop(st);
                    switch_plan(target.id, server_writer).await?;
                }
            } else {
                drop(st);
                send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
            }
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => {
            if plans_len > 1 {
                let next_idx = (st.plans_view.selected_plan_index + 1) % plans_len;
                st.plans_view.selected_plan_index = next_idx;
                if let Some(target) = st.plans_view.plans_list.get(next_idx).cloned() {
                    st.plans_view.active_plan_id = Some(target.id.clone());
                    st.plans_view.reset_view_for_new_plan();
                    drop(st);
                    switch_plan(target.id, server_writer).await?;
                }
            } else {
                drop(st);
                send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
            }
        }
        KeyCode::Char('x') | KeyCode::Char('X') => {
            let plan_id = st.plans_view.current_plan.as_ref().map(|p| p.id.clone());
            let sel_row = st.plans_view.selected_row();
            if let (Some(plan_id), Some(row)) = (plan_id, sel_row) {
                let new_checked = !row.checked;
                if let Some(plan) = st.plans_view.current_plan.as_mut() {
                    plan.toggle_item_checked(&row.item_id);
                }
                drop(st);
                let params = PlanUpdateItemParams {
                    plan_id,
                    item_id: row.item_id,
                    status: None,
                    checked: Some(new_checked),
                };
                send_request(server_writer, methods::PLAN_UPDATE_ITEM, serde_json::to_value(params)?).await?;
            }
        }
        KeyCode::Char('t') | KeyCode::Char('T') => {
            let plan_id = st.plans_view.current_plan.as_ref().map(|p| p.id.clone());
            let sel_row = st.plans_view.selected_row();
            if let (Some(plan_id), Some(row)) = (plan_id, sel_row) {
                let next_status = row.status.next();
                if let Some(plan) = st.plans_view.current_plan.as_mut() {
                    plan.update_item_status(&row.item_id, next_status);
                }
                drop(st);
                let params = PlanUpdateItemParams {
                    plan_id,
                    item_id: row.item_id,
                    status: Some(next_status),
                    checked: None,
                };
                send_request(server_writer, methods::PLAN_UPDATE_ITEM, serde_json::to_value(params)?).await?;
            }
        }
        KeyCode::Delete | KeyCode::Char('D') => {
            if let Some(plan) = &st.plans_view.current_plan {
                st.confirm_delete_plan = Some(plan.id.clone());
            }
        }
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('y') | KeyCode::Char('Y') => {
            if let Some(plan) = &st.plans_view.current_plan {
                let md = format!("# Plan: {}\n\n{}\n", plan.title, plan.description.as_deref().unwrap_or(""));
                let msg = if crate::clipboard::copy_to_clipboard(&md) {
                    "Plan copied to clipboard"
                } else {
                    "Failed to copy plan to clipboard"
                };
                st.model.copy_notification = Some((msg.to_string(), std::time::Instant::now()));
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            let active_id = st.plans_view.active_plan_id.clone().or_else(|| {
                st.plans_view
                    .plans_list
                    .get(st.plans_view.selected_plan_index)
                    .map(|p| p.id.clone())
            });
            drop(st);
            send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
            let get_params = match active_id {
                Some(id) => serde_json::json!({ "id": id }),
                None => serde_json::json!({}),
            };
            send_request(server_writer, methods::PLAN_GET, get_params).await?;
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
