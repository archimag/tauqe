use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::methods;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::app::{AppState, KeyCommand, ViewMode};
use crate::editor::InputEditor;
use crate::input::InputResult;
use crate::rpc::send_request;
use crate::ui::plans::VisiblePlanRow;

pub const PLANS_COMMANDS: &[KeyCommand] = &[
    KeyCommand { key: "n / p (or ↑/↓)", description: "Navigate plans and items" },
    KeyCommand { key: "C-n / C-p", description: "Inspect next / previous item (accordion walk)" },
    KeyCommand { key: "Tab / Space", description: "Fold / unfold plan or item subtasks" },
    KeyCommand { key: "x", description: "Execute target step, group, or entire plan (with confirmation)" },
    KeyCommand { key: "Enter", description: "Execute leaf step; fold/unfold on groups and plan headers" },
    KeyCommand { key: "d", description: "Discuss selected step or entire plan with AI in Develop" },
    KeyCommand { key: "R", description: "Deep refine plan architecture & steps with AI (Architect)" },
    KeyCommand { key: "t / s", description: "Change item status (Todo / InProgress / Done / Cancelled)" },
    KeyCommand { key: "a", description: "Toggle fold / unfold all plans and items" },
    KeyCommand { key: "Delete", description: "Delete plan (with confirmation)" },
    KeyCommand { key: "c / y", description: "Copy plan as Markdown to clipboard" },
    KeyCommand { key: "PgUp / PgDn", description: "Page scroll plan view" },
    KeyCommand { key: "Home / End", description: "Select first / last row" },
    KeyCommand { key: "r", description: "Refresh plans from server" },
    KeyCommand { key: "Esc / q", description: "Return to Develop view" },
];

fn execute_scope_action(st: &mut AppState, plan_id: &str, scope_id: Option<&str>, scope_title: &str) {
    if st.model.is_busy() || st.review.running {
        st.notify_warning("Model is currently busy; wait for turn to finish or press Esc to cancel");
        return;
    }

    let plan = match st.plans_view.plans.iter().find(|p| p.id == plan_id).cloned() {
        Some(p) => p,
        None => {
            st.notify_error(format!("Plan '{}' not found", plan_id));
            return;
        }
    };

    match plan.evaluate_scope(scope_id) {
        tauqe_protocol::PlanScopeEvaluation::BlockedByDiscussion(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(format!(
                "Scope contains {} item(s) in DISCUSSION ({}). Review or approve before executing.",
                items.len(), preview
            ));
        }
        tauqe_protocol::PlanScopeEvaluation::AllCompleted => {
            st.notify_info("All steps in this scope are already completed.");
        }
        tauqe_protocol::PlanScopeEvaluation::Cancelled => {
            st.notify_warning("Step is CANCELLED; change status to TODO before executing.");
        }
        tauqe_protocol::PlanScopeEvaluation::NotFound => {
            st.notify_error("Target item not found in plan.");
        }
        tauqe_protocol::PlanScopeEvaluation::Ready(steps) => {
            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            st.confirm_execute_scope = Some(crate::app::ConfirmExecuteScopeState {
                plan_id: plan.id.clone(),
                plan_title: plan.title.clone(),
                scope_title: scope_title.to_string(),
                steps,
            });
        }
    }
}

pub async fn handle_plans_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut ChildStdin,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    let primary_mod = st.tui_config.input.primary_modifier;
    let is_ctrl_alt = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        && key.modifiers.contains(crossterm::event::KeyModifiers::ALT);
    let is_primary = !is_ctrl_alt && primary_mod.matches(key.modifiers);
    let is_ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);

    let is_ctrl_n = !is_ctrl_alt
        && (is_ctrl || is_primary)
        && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'));
    let is_ctrl_p = !is_ctrl_alt
        && (is_ctrl || is_primary)
        && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'));

    if is_ctrl_n {
        st.plans_view.accordion_navigate(true);
        return Ok(InputResult::Continue);
    }
    if is_ctrl_p {
        st.plans_view.accordion_navigate(false);
        return Ok(InputResult::Continue);
    }

    let rows_len = st.plans_view.flatten_rows().len();

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            st.view_mode = ViewMode::Develop;
        }
        KeyCode::Up | KeyCode::Char('p') | KeyCode::Char('P') if !is_ctrl && !is_primary => {
            if st.plans_view.selected_index > 0 {
                st.plans_view.selected_index -= 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::Down | KeyCode::Char('n') | KeyCode::Char('N') if !is_ctrl && !is_primary => {
            if rows_len > 0 && st.plans_view.selected_index + 1 < rows_len {
                st.plans_view.selected_index += 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::Home => {
            st.plans_view.selected_index = 0;
            st.plans_view.scroll_to_selected();
        }
        KeyCode::End => {
            if rows_len > 0 {
                st.plans_view.selected_index = rows_len - 1;
                st.plans_view.scroll_to_selected();
            }
        }
        KeyCode::PageUp => {
            let page = st.plans_view.view_height.saturating_sub(2).max(1);
            st.plans_view.scroll = st.plans_view.scroll.saturating_sub(page);
        }
        KeyCode::PageDown => {
            let page = st.plans_view.view_height.saturating_sub(2).max(1);
            let max = (st.plans_view.rendered_lines as u16).saturating_sub(st.plans_view.view_height);
            st.plans_view.scroll = st.plans_view.scroll.saturating_add(page).min(max);
        }
        KeyCode::Tab | KeyCode::Char(' ') => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, .. } => {
                        st.plans_view.toggle_plan_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::PlanItem { plan_id, item_id, is_expandable, .. } => {
                        if is_expandable {
                            st.plans_view.toggle_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                        }
                    }
                }
            }
        }
        KeyCode::Enter => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, .. } => {
                        st.plans_view.toggle_plan_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::PlanItem {
                        plan_id,
                        item_id,
                        title,
                        has_children,
                        ..
                    } => {
                        if has_children {
                            st.plans_view.toggle_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                        } else {
                            execute_scope_action(&mut st, &plan_id, Some(&item_id), &title);
                        }
                    }
                }
            }
        }
        KeyCode::Char('x') | KeyCode::Char('X') => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, title, .. } => {
                        execute_scope_action(&mut st, &plan_id, None, &title);
                    }
                    VisiblePlanRow::PlanItem {
                        plan_id,
                        item_id,
                        title,
                        ..
                    } => {
                        execute_scope_action(&mut st, &plan_id, Some(&item_id), &title);
                    }
                }
            }
        }
        KeyCode::Char('d') | KeyCode::Char('D') => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == plan_id).cloned() {
                    let mut focused = Vec::new();
                    if let VisiblePlanRow::PlanItem { item_id, title, .. } = row {
                        focused.push((item_id, title));
                    }

                    st.discuss_plan_dialog = Some(crate::app::DiscussPlanDialogState {
                        plan_id: plan.id.clone(),
                        plan_title: plan.title.clone(),
                        focused_items: focused,
                        prompt_editor: InputEditor::default(),
                    });
                }
            }
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            st.plans_view.toggle_fold_all();
            st.plans_view.clamp_selection();
            st.plans_view.scroll_to_selected();
        }
        KeyCode::Char('t') | KeyCode::Char('T') | KeyCode::Char('s') | KeyCode::Char('S') => {
            let sel_row = st.plans_view.selected_row();
            if let Some(VisiblePlanRow::PlanItem { plan_id, item_id, title, status, .. }) = sel_row {
                let cur_idx = match status {
                    tauqe_protocol::PlanItemStatus::Discussion => 0,
                    tauqe_protocol::PlanItemStatus::Todo => 1,
                    tauqe_protocol::PlanItemStatus::InProgress => 2,
                    tauqe_protocol::PlanItemStatus::Done => 3,
                    tauqe_protocol::PlanItemStatus::Cancelled => 4,
                };
                st.status_dialog = Some(crate::app::StatusDialogState {
                    target: crate::app::StatusDialogTarget::PlanItem {
                        plan_id,
                        item_id,
                        item_title: title,
                        current_status: status,
                    },
                    selected_index: cur_idx,
                });
            }
        }
        KeyCode::Delete => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                st.confirm_delete_plan = Some(row.plan_id().to_string());
            }
        }
        KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('y') | KeyCode::Char('Y') => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id();
                if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == plan_id) {
                    let md = plan.to_markdown();
                    drop(st);
                    let res = crate::clipboard::copy_to_clipboard(&md);
                    let mut st = state.lock().await;
                    match res {
                        crate::clipboard::CopyResult::Native => {
                            st.notify_success("Plan copied to clipboard");
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info("Plan sent to terminal clipboard (OSC 52)");
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error("Failed to copy plan to clipboard");
                        }
                    }
                }
            }
        }
        KeyCode::Char('R') => {
            if st.model.is_busy() || st.review.running || st.plans_view.refining {
                st.notify_warning("Model is currently busy; wait for operation to finish");
                return Ok(InputResult::Continue);
            }
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == plan_id).cloned() {
                    let mut models = vec![
                        (format!("Default ({})", st.active_model), None),
                    ];
                    let senior_label = if let Some(tiers) = &st.model_choice.tiers {
                        format!("Senior Architect ({})", tiers.senior)
                    } else {
                        "Senior Architect (tier)".to_string()
                    };
                    models.push((
                        senior_label,
                        Some(tauqe_protocol::ModelSelection::Tier(tauqe_protocol::ModelTier::Senior)),
                    ));

                    st.plans_view.refine_dialog = Some(crate::ui::plans::PlanRefineDialogState {
                        plan_id: plan.id,
                        plan_title: plan.title,
                        models,
                        selected_model_index: 0,
                        instructions_editor: InputEditor::default(),
                        confirm_button: crate::app::ConfirmDialogButton::Cancel,
                    });
                }
            }
        }
        KeyCode::Char('r') => {
            drop(st);
            send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
        }
        _ => {}
    }

    Ok(InputResult::Continue)
}
