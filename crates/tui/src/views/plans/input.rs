use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::methods;
use tokio::sync::Mutex;

use crate::app::{AppState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::VisiblePlanRow;

/// Keyboard actions available in the Plans view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlansAction {
    Return,
    AccordionNext,
    AccordionPrev,
    NavPrev,
    NavNext,
    SelectFirst,
    SelectLast,
    PageUp,
    PageDown,
    FoldToggle,
    Activate,
    Execute,
    Discuss,
    SetFocus,
    FoldAll,
    ChangeStatus,
    DeletePlan,
    CopyMarkdown,
    Replan,
    Refresh,
}

/// Single source of truth for the Plans view keybindings.
///
/// Essential commands drive plan execution and the workflow cycle; Interface
/// commands navigate, fold, and scroll the tree; Helpers copy, refresh, or
/// delete. The map drives both input dispatch and the generated Help dialog.
pub static PLANS_COMMAND_MAP: CommandMap<PlansAction> = CommandMap::new(&[
    // Essential: execution and workflow.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        PlansAction::Activate,
        Essential,
        "Execute a leaf step, or fold groups and headers",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('x')),
        PlansAction::Execute,
        Essential,
        "Execute the selected step, group, or entire plan",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('X')),
        PlansAction::Execute,
        Essential,
        "Execute the selected step, group, or entire plan",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('R')),
        PlansAction::Replan,
        Essential,
        "Refine or replan the horizon with the Architect",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('d')),
        PlansAction::Discuss,
        Essential,
        "Discuss the selection with AI in the REPL",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('D')),
        PlansAction::Discuss,
        Essential,
        "Discuss the selection with AI in the REPL",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('t')),
        PlansAction::ChangeStatus,
        Essential,
        "Change the step status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('T')),
        PlansAction::ChangeStatus,
        Essential,
        "Change the step status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('s')),
        PlansAction::ChangeStatus,
        Essential,
        "Change the step status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('S')),
        PlansAction::ChangeStatus,
        Essential,
        "Change the step status",
    ),
    // Interface: navigation, accordion walk, folding, and scrolling.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        PlansAction::Return,
        Interface,
        "Return to the REPL",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        PlansAction::Return,
        Interface,
        "Return to the REPL",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('n')),
        PlansAction::AccordionNext,
        Interface,
        "Walk to the next item (accordion)",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('n')),
        PlansAction::AccordionNext,
        Interface,
        "Walk to the next item (accordion)",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('p')),
        PlansAction::AccordionPrev,
        Interface,
        "Walk to the previous item (accordion)",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('p')),
        PlansAction::AccordionPrev,
        Interface,
        "Walk to the previous item (accordion)",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('n')),
        PlansAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('p')),
        PlansAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        PlansAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        PlansAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('N')),
        PlansAction::NavNext,
        Interface,
        "Select the next row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        PlansAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        PlansAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('P')),
        PlansAction::NavPrev,
        Interface,
        "Select the previous row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        PlansAction::SelectFirst,
        Interface,
        "Select the first row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        PlansAction::SelectLast,
        Interface,
        "Select the last row",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        PlansAction::FoldToggle,
        Interface,
        "Fold or unfold the selected plan, section, or step",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        PlansAction::FoldToggle,
        Interface,
        "Fold or unfold the selected plan, section, or step",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        PlansAction::FoldAll,
        Interface,
        "Toggle fold or unfold for all plans, sections, and items",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('A')),
        PlansAction::FoldAll,
        Interface,
        "Toggle fold or unfold for all plans, sections, and items",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        PlansAction::PageUp,
        Interface,
        "Scroll the plan view up a page",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        PlansAction::PageDown,
        Interface,
        "Scroll the plan view down a page",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('f')),
        PlansAction::SetFocus,
        Interface,
        "Set the discussion focus to the selection without leaving the view",
    ),
    // Helpers: clipboard, refresh, and destructive operations.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        PlansAction::CopyMarkdown,
        Helpers,
        "Copy the plan as Markdown to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('C')),
        PlansAction::CopyMarkdown,
        Helpers,
        "Copy the plan as Markdown to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        PlansAction::CopyMarkdown,
        Helpers,
        "Copy the plan as Markdown to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('Y')),
        PlansAction::CopyMarkdown,
        Helpers,
        "Copy the plan as Markdown to the clipboard",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('r')),
        PlansAction::Refresh,
        Helpers,
        "Refresh plans from the server",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        PlansAction::DeletePlan,
        Helpers,
        "Delete the plan",
    ),
]);

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
        tauqe_protocol::PlanScopeEvaluation::BlockedByBlockers(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(format!(
                "Scope contains {} step(s) in BLOCKED status ({}). Resolve blockers or press 'R' to replan horizon before executing.",
                items.len(), preview
            ));
        }
        tauqe_protocol::PlanScopeEvaluation::AllCompleted => {
            st.notify_info("All steps in this scope are already completed.");
        }
        tauqe_protocol::PlanScopeEvaluation::BlockedByDependencies(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(format!(
                "Scope contains {} step(s) with unmet dependencies ({}). Complete prerequisites first.",
                items.len(), preview
            ));
        }
        tauqe_protocol::PlanScopeEvaluation::StepBlocked => {
            st.notify_warning("Step is BLOCKED; resolve the blocker or press 'R' to replan horizon before executing.");
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
                step_id: scope_id.map(str::to_string),
                steps,
            });
        }
    }
}

pub async fn handle_plans_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    let action = match PLANS_COMMAND_MAP.resolve(key, &st.tui_config.input, false) {
        Some(spec) => spec.action,
        None => return Ok(InputResult::Continue),
    };

    match action {
        PlansAction::Return => {
            st.view_mode = ViewMode::Develop;
        }
        PlansAction::AccordionNext => {
            st.plans_view.accordion_navigate(true);
        }
        PlansAction::AccordionPrev => {
            st.plans_view.accordion_navigate(false);
        }
        PlansAction::NavPrev => {
            if st.plans_view.selected_index > 0 {
                st.plans_view.selected_index -= 1;
                st.plans_view.scroll_to_selected();
            }
        }
        PlansAction::NavNext => {
            let rows_len = st.plans_view.flatten_rows().len();
            if rows_len > 0 && st.plans_view.selected_index + 1 < rows_len {
                st.plans_view.selected_index += 1;
                st.plans_view.scroll_to_selected();
            }
        }
        PlansAction::SelectFirst => {
            st.plans_view.selected_index = 0;
            st.plans_view.scroll_to_selected();
        }
        PlansAction::SelectLast => {
            let rows_len = st.plans_view.flatten_rows().len();
            if rows_len > 0 {
                st.plans_view.selected_index = rows_len - 1;
                st.plans_view.scroll_to_selected();
            }
        }
        PlansAction::PageUp => {
            let page = st.plans_view.view_height.saturating_sub(2).max(1);
            st.plans_view.scroll = st.plans_view.scroll.saturating_sub(page);
        }
        PlansAction::PageDown => {
            let page = st.plans_view.view_height.saturating_sub(2).max(1);
            let max = (st.plans_view.rendered_lines as u16)
                .saturating_sub(st.plans_view.view_height);
            st.plans_view.scroll = st.plans_view.scroll.saturating_add(page).min(max);
        }
        PlansAction::FoldToggle => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, .. } => {
                        st.plans_view.toggle_plan_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::ConceptHeader { plan_id, .. } => {
                        st.plans_view.toggle_concept_section_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::ConceptItem { plan_id, item_id, is_expandable, .. } => {
                        if is_expandable {
                            st.plans_view.toggle_concept_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                        }
                    }
                    VisiblePlanRow::ImplementationHeader { plan_id, .. } => {
                        st.plans_view.toggle_impl_section_expanded(&plan_id);
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
        PlansAction::Activate => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, .. } => {
                        st.plans_view.toggle_plan_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::ConceptHeader { plan_id, .. } => {
                        st.plans_view.toggle_concept_section_expanded(&plan_id);
                        st.plans_view.clamp_selection();
                        st.plans_view.scroll_to_selected();
                    }
                    VisiblePlanRow::ConceptItem {
                        plan_id,
                        item_id,
                        is_expandable,
                        ..
                    } => {
                        if is_expandable {
                            st.plans_view.toggle_concept_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                        } else {
                            st.notify_info("Concept items define architectural requirements. Press 'd' to discuss in Develop.");
                        }
                    }
                    VisiblePlanRow::ImplementationHeader { plan_id, .. } => {
                        st.plans_view.toggle_impl_section_expanded(&plan_id);
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
        PlansAction::Execute => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisiblePlanRow::PlanHeader { plan_id, title, .. } => {
                        execute_scope_action(&mut st, &plan_id, None, &title);
                    }
                    VisiblePlanRow::ConceptHeader { .. } => {
                        st.notify_warning("Concept section cannot be executed; execute implementation steps below.");
                    }
                    VisiblePlanRow::ConceptItem { .. } => {
                        st.notify_warning("Concept item cannot be executed directly; execute implementation steps or press 'd' to discuss.");
                    }
                    VisiblePlanRow::ImplementationHeader { plan_id, .. } => {
                        execute_scope_action(&mut st, &plan_id, None, "Implementation Scope");
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
        PlansAction::Discuss => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == plan_id).cloned() {
                    let mut focused = Vec::new();
                    match row {
                        VisiblePlanRow::ConceptItem { item_id, title, .. } => {
                            focused.push((item_id, title));
                        }
                        VisiblePlanRow::PlanItem { item_id, title, .. } => {
                            focused.push((item_id, title));
                        }
                        _ => {}
                    }

                    st.prompt_modal = Some(crate::app::PromptModalState::new(
                        crate::app::PromptModalTarget::Plan {
                            plan_id: plan.id.clone(),
                            plan_title: plan.title.clone(),
                            focused_items: focused,
                        },
                    ));
                }
            }
        }
        PlansAction::SetFocus => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                let plan_title = st
                    .plans_view
                    .plans
                    .iter()
                    .find(|p| p.id == plan_id)
                    .map(|p| p.title.clone())
                    .unwrap_or_else(|| plan_id.clone());
                let (focused, target_label) = match &row {
                    VisiblePlanRow::ConceptItem { item_id, title, .. } => {
                        (vec![item_id.clone()], Some(format!("concept {} '{}'", item_id, title)))
                    }
                    VisiblePlanRow::PlanItem { item_id, title, .. } => {
                        (vec![item_id.clone()], Some(format!("step {} '{}'", item_id, title)))
                    }
                    _ => (Vec::new(), None),
                };
                let focus = tauqe_protocol::DiscussionFocus::plan(plan_id, focused);
                st.discussion_focus = focus.clone();
                let params = tauqe_protocol::DiscussionSetFocusParams { focus };
                let message = match target_label {
                    Some(label) => {
                        format!("Discussion focus set to {} in plan '{}'", label, plan_title)
                    }
                    None => format!("Discussion focus set to plan '{}'", plan_title),
                };
                drop(st);
                send_request(
                    server_writer,
                    methods::DISCUSSION_SET_FOCUS,
                    serde_json::to_value(&params)?,
                )
                .await?;
                let mut st = state.lock().await;
                st.notify_info(message);
            }
        }
        PlansAction::FoldAll => {
            st.plans_view.toggle_fold_all();
            st.plans_view.clamp_selection();
            st.plans_view.scroll_to_selected();
        }
        PlansAction::ChangeStatus => {
            let sel_row = st.plans_view.selected_row();
            match sel_row {
                Some(VisiblePlanRow::PlanItem { plan_id, item_id, title, status, .. }) => {
                    let cur_idx = match status {
                        tauqe_protocol::PlanItemStatus::Discussion => 0,
                        tauqe_protocol::PlanItemStatus::Todo => 1,
                        tauqe_protocol::PlanItemStatus::InProgress => 2,
                        tauqe_protocol::PlanItemStatus::Done => 3,
                        tauqe_protocol::PlanItemStatus::Blocked => 4,
                        tauqe_protocol::PlanItemStatus::Cancelled => 5,
                    };
                    st.status_dialog = Some(crate::app::StatusDialogState::new(
                        crate::app::StatusDialogTarget::PlanItem {
                            plan_id,
                            item_id,
                            item_title: title,
                            current_status: status,
                        },
                        cur_idx,
                    ));
                }
                Some(VisiblePlanRow::ConceptItem { .. }) => {
                    st.notify_info("Concept items define architectural requirements and do not have task statuses. Press 'd' to discuss.");
                }
                _ => {}
            }
        }
        PlansAction::DeletePlan => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                st.confirm_delete_plan = Some(row.plan_id().to_string());
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        PlansAction::CopyMarkdown => {
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
        PlansAction::Replan => {
            if st.model.is_busy() || st.review.running || st.plans_view.refining {
                st.notify_warning("Model is currently busy; wait for operation to finish");
                return Ok(InputResult::Continue);
            }
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st.plans_view.plans.iter().find(|p| p.id == plan_id).cloned() {
                    // Prefer the configured Architect entries; fall back to the
                    // resolved Architect role (or the active model) when none
                    // are configured, so the dialog always offers a valid pick.
                    let architect_entries = st
                        .model_choice
                        .roles
                        .as_ref()
                        .and_then(|roles| roles.entries.as_ref())
                        .map(|entries| entries.architect.clone())
                        .filter(|entries| !entries.is_empty())
                        .unwrap_or_else(|| {
                            let fallback_model = st
                                .model_choice
                                .resolve(&tauqe_protocol::ModelSelection::Role(
                                    tauqe_protocol::ModelRole::Architect,
                                ))
                                .unwrap_or_else(|| st.active_model.clone());
                            let fallback_effort = st
                                .model_choice
                                .roles
                                .as_ref()
                                .and_then(|roles| roles.efforts.as_ref())
                                .and_then(|efforts| efforts.architect);
                            vec![tauqe_protocol::ModelEntry::new(
                                fallback_model,
                                fallback_effort,
                                None,
                            )]
                        });

                    let mut dialog = super::PlanRefineDialogState::new(
                        plan.id,
                        plan.title,
                        architect_entries,
                    );
                    if let VisiblePlanRow::PlanItem { item_id, title, status: tauqe_protocol::PlanItemStatus::Blocked, .. } = &row {
                        dialog.instructions_editor.insert_str(&format!(
                            "Replan uncompleted horizon around blocked step #{}: {}",
                            item_id, title
                        ));
                    }
                    // Open on the architect list when more than one model is
                    // configured and instructions are empty so it can be picked
                    // instantly with the digit shortcuts; if pre-seeded with
                    // instructions, keep focus on the editor.
                    if dialog.selector.len() > 1 && dialog.instructions_editor.is_empty() {
                        dialog.focus = super::RefineDialogFocus::ModelList;
                    }
                    st.plans_view.refine_dialog = Some(dialog);
                }
            }
        }
        PlansAction::Refresh => {
            drop(st);
            send_request(server_writer, methods::PLAN_LIST, serde_json::json!({})).await?;
        }
    }

    Ok(InputResult::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_map_chords_are_unique() {
        let chords: Vec<KeyChord> = PLANS_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in PLANS_COMMAND_MAP");
            }
        }
    }

    #[tokio::test]
    async fn test_r_key_on_blocked_step_seeds_replan_instructions() {
        let mut st = crate::app::test_state();
        let plan = tauqe_protocol::Plan {
            id: "plan-blocked".to_string(),
            title: "Blocked Plan".to_string(),
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Failing step".to_string(),
                status: tauqe_protocol::PlanItemStatus::Blocked,
                ..Default::default()
            }],
            ..Default::default()
        };
        st.plans_view.plans = vec![plan];
        st.plans_view.expand_all();
        st.plans_view.selected_index = 1; // Step 1

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_plans_key(
            KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        assert!(matches!(res, InputResult::Continue));
        let guard = state.lock().await;
        let dialog = guard.plans_view.refine_dialog.as_ref().expect("dialog opened");
        assert!(dialog
            .instructions_editor
            .get_text()
            .contains("Replan uncompleted horizon around blocked step #1: Failing step"));
        assert!(dialog.focus.is_instructions_editor());
    }
}
