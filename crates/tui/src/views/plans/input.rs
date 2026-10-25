use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::methods;
use tokio::sync::Mutex;

use crate::app::{AppState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::i18n::engine::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::{architect_entries, VisiblePlanRow};

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
    Recenter,
    PageUp,
    PageDown,
    FoldToggle,
    Activate,
    Execute,
    ExecuteAll,
    Discuss,
    SetFocus,
    FoldAll,
    ChangeStatus,
    DeletePlan,
    CopyMarkdown,
    ClearHorizon,
    Replan,
    ReplanAll,
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
        "cmd.plans.activate",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('x')),
        PlansAction::Execute,
        Essential,
        "cmd.plans.execute",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('X')),
        PlansAction::ExecuteAll,
        Essential,
        "cmd.plans.execute_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('r')),
        PlansAction::Replan,
        Essential,
        "cmd.plans.replan",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('R')),
        PlansAction::ReplanAll,
        Essential,
        "cmd.plans.replan_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('d')),
        PlansAction::Discuss,
        Essential,
        "cmd.plans.discuss",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('D')),
        PlansAction::Discuss,
        Essential,
        "cmd.plans.discuss",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('t')),
        PlansAction::ChangeStatus,
        Essential,
        "cmd.plans.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('T')),
        PlansAction::ChangeStatus,
        Essential,
        "cmd.plans.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('s')),
        PlansAction::ChangeStatus,
        Essential,
        "cmd.plans.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('S')),
        PlansAction::ChangeStatus,
        Essential,
        "cmd.plans.change_status",
    ),
    // Interface: navigation, accordion walk, folding, and scrolling.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        PlansAction::Return,
        Interface,
        "cmd.plans.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        PlansAction::Return,
        Interface,
        "cmd.plans.return",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('n')),
        PlansAction::AccordionNext,
        Interface,
        "cmd.plans.accordion_next",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('n')),
        PlansAction::AccordionNext,
        Interface,
        "cmd.plans.accordion_next",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('p')),
        PlansAction::AccordionPrev,
        Interface,
        "cmd.plans.accordion_prev",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('p')),
        PlansAction::AccordionPrev,
        Interface,
        "cmd.plans.accordion_prev",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('n')),
        PlansAction::NavNext,
        Interface,
        "cmd.plans.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Alt(KeyCode::Char('p')),
        PlansAction::NavPrev,
        Interface,
        "cmd.plans.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        PlansAction::NavNext,
        Interface,
        "cmd.plans.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        PlansAction::NavNext,
        Interface,
        "cmd.plans.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('N')),
        PlansAction::NavNext,
        Interface,
        "cmd.plans.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        PlansAction::NavPrev,
        Interface,
        "cmd.plans.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        PlansAction::NavPrev,
        Interface,
        "cmd.plans.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('P')),
        PlansAction::NavPrev,
        Interface,
        "cmd.plans.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        PlansAction::SelectFirst,
        Interface,
        "cmd.plans.select_first",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        PlansAction::SelectFirst,
        Interface,
        "cmd.plans.select_first",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        PlansAction::SelectFirst,
        Interface,
        "cmd.plans.select_first",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        PlansAction::SelectLast,
        Interface,
        "cmd.plans.select_last",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        PlansAction::SelectLast,
        Interface,
        "cmd.plans.select_last",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        PlansAction::SelectLast,
        Interface,
        "cmd.plans.select_last",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('l')),
        PlansAction::Recenter,
        Interface,
        "cmd.plans.recenter",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('l')),
        PlansAction::Recenter,
        Interface,
        "cmd.plans.recenter",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        PlansAction::FoldToggle,
        Interface,
        "cmd.plans.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        PlansAction::FoldToggle,
        Interface,
        "cmd.plans.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        PlansAction::FoldAll,
        Interface,
        "cmd.plans.fold_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('A')),
        PlansAction::FoldAll,
        Interface,
        "cmd.plans.fold_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        PlansAction::PageUp,
        Interface,
        "cmd.plans.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        PlansAction::PageDown,
        Interface,
        "cmd.plans.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('f')),
        PlansAction::SetFocus,
        Interface,
        "cmd.plans.set_focus",
    ),
    // Helpers: clipboard, refresh, and destructive operations.
    //
    // Copy (`c` / `y`) and the destructive mass clear (`Shift+C`) are declared as
    // disjoint chord sets so the two verbs never compete for the same key event
    // (Conventions §4.4). Every copy chord is grouped first and every clear chord
    // second; the grouping is documentation only, because resolution depends
    // solely on the chord and never on its position in the map.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        PlansAction::CopyMarkdown,
        Helpers,
        "cmd.plans.copy_markdown",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        PlansAction::CopyMarkdown,
        Helpers,
        "cmd.plans.copy_markdown",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('Y')),
        PlansAction::CopyMarkdown,
        Helpers,
        "cmd.plans.copy_markdown",
    ),
    // `Shift+C` is the mass clear of the plan horizon. It is registered under
    // the uppercase bare chord and the raw `Shift` chord because terminals
    // report a shifted letter inconsistently; both spellings resolve to the same
    // destructive action and never to the copy binding (Conventions §4.4).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('C')),
        PlansAction::ClearHorizon,
        Helpers,
        "cmd.plans.clear_horizon",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('c')),
        PlansAction::ClearHorizon,
        Helpers,
        "cmd.plans.clear_horizon",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('g')),
        PlansAction::Refresh,
        Helpers,
        "cmd.plans.refresh",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('G')),
        PlansAction::Refresh,
        Helpers,
        "cmd.plans.refresh",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        PlansAction::DeletePlan,
        Helpers,
        "cmd.plans.delete",
    ),
]);

fn execute_scope_action(
    st: &mut AppState,
    plan_id: &str,
    scope_id: Option<&str>,
    scope_title: &str,
) {
    if st.model.is_busy() || st.review.running {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let plan = match st
        .plans_view
        .plans
        .iter()
        .find(|p| p.id == plan_id)
        .cloned()
    {
        Some(p) => p,
        None => {
            st.notify_error(active_t("notify.plan_not_found").replace("{}", plan_id));
            return;
        }
    };

    // A plan that carries only its architectural concept has no synthesized
    // horizon yet: the batch projects the first horizon just-in-time, so the
    // confirmation arms with an empty queue instead of reporting a finished
    // scope (c-5).
    if plan.items.is_empty() && !plan.concept.is_empty() && scope_id.is_none() {
        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
        st.confirm_execute_scope = Some(crate::app::ConfirmExecuteScopeState {
            plan_id: plan.id.clone(),
            plan_title: plan.title.clone(),
            scope_title: scope_title.to_string(),
            step_id: None,
            steps: Vec::new(),
        });
        return;
    }

    match plan.evaluate_scope(scope_id) {
        tauqe_protocol::PlanScopeEvaluation::BlockedByDiscussion(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(
                active_t("notify.plan_scope_discussion")
                    .replacen("{}", &items.len().to_string(), 1)
                    .replace("{}", &preview),
            );
        }
        tauqe_protocol::PlanScopeEvaluation::BlockedByBlockers(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(
                active_t("notify.plan_scope_blocked")
                    .replacen("{}", &items.len().to_string(), 1)
                    .replace("{}", &preview),
            );
        }
        tauqe_protocol::PlanScopeEvaluation::AllCompleted => {
            st.notify_info(active_t("notify.plan_scope_all_completed"));
        }
        tauqe_protocol::PlanScopeEvaluation::BlockedByDependencies(items) => {
            let preview = if items.len() <= 2 {
                items.join(", ")
            } else {
                format!("{}, and {} more", items[..2].join(", "), items.len() - 2)
            };
            st.notify_warning(
                active_t("notify.plan_scope_dependencies")
                    .replacen("{}", &items.len().to_string(), 1)
                    .replace("{}", &preview),
            );
        }
        tauqe_protocol::PlanScopeEvaluation::StepBlocked => {
            st.notify_warning(active_t("notify.plan_step_blocked"));
        }
        tauqe_protocol::PlanScopeEvaluation::Cancelled => {
            st.notify_warning(active_t("notify.plan_step_cancelled"));
        }
        tauqe_protocol::PlanScopeEvaluation::NotFound => {
            st.notify_error(active_t("notify.plan_item_not_found"));
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

/// Opens the Architect refine dialog for a plan, optionally pre-seeding the
/// instructions. When more than one architect is configured and no instructions
/// were seeded, focus starts on the architect list for instant digit selection.
fn open_refine_dialog(st: &mut AppState, plan: tauqe_protocol::Plan, seed: Option<String>) {
    let entries = architect_entries(st);
    let mut dialog = super::PlanRefineDialogState::new(plan.id, plan.title, entries);
    if let Some(seed) = seed {
        dialog.instructions_editor.insert_str(&seed);
    }
    if dialog.selector.len() > 1 && dialog.instructions_editor.is_empty() {
        dialog.focus = super::RefineDialogFocus::ModelList;
    }
    st.plans_view.refine_dialog = Some(dialog);
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
            st.view_mode = ViewMode::Repl;
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
            let max =
                (st.plans_view.rendered_lines as u16).saturating_sub(st.plans_view.view_height);
            st.plans_view.scroll = st.plans_view.scroll.saturating_add(page).min(max);
        }
        PlansAction::Recenter => {
            st.plans_view.recenter_selected();
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
                    VisiblePlanRow::ConceptItem {
                        plan_id,
                        item_id,
                        is_expandable,
                        ..
                    } => {
                        if is_expandable {
                            st.plans_view
                                .toggle_concept_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
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
                        is_expandable,
                        ..
                    } => {
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
                            st.plans_view
                                .toggle_concept_item_expanded(&plan_id, &item_id);
                            st.plans_view.clamp_selection();
                            st.plans_view.scroll_to_selected();
                        } else {
                            st.notify_info(active_t("notify.plan_concept_discuss"));
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
                        st.notify_warning(active_t("notify.plan_concept_section_not_executable"));
                    }
                    VisiblePlanRow::ConceptItem { .. } => {
                        st.notify_warning(active_t("notify.plan_concept_item_not_executable"));
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
        PlansAction::ExecuteAll => {
            // `X` executes every ready plan as one pipeline, regardless of which
            // row the cursor rests on: the queue is assembled from every plan
            // that carries a ready leaf horizon or only its architectural
            // concept, and the whole queue runs under one registered operation
            // (c-1, c-5).
            let plans: Vec<tauqe_protocol::Plan> = st.plans_view.plans.clone();
            let mut plan_ids = Vec::new();
            let mut plan_titles = Vec::new();
            let mut steps = Vec::new();
            let mut pending_synthesis = Vec::new();

            for plan in &plans {
                if plan.items.is_empty() && !plan.concept.is_empty() {
                    // Concept-only plan: the batch projects its first horizon
                    // just-in-time before the queue reaches it.
                    plan_ids.push(plan.id.clone());
                    plan_titles.push(plan.title.clone());
                    pending_synthesis.push(plan.title.clone());
                    continue;
                }
                if let tauqe_protocol::PlanScopeEvaluation::Ready(ready) = plan.evaluate_scope(None)
                {
                    plan_ids.push(plan.id.clone());
                    plan_titles.push(plan.title.clone());
                    steps.extend(ready);
                }
            }

            if plan_ids.is_empty() {
                st.notify_info(active_t("notify.plan_pipeline_empty"));
                return Ok(InputResult::Continue);
            }

            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            st.confirm_execute_pipeline = Some(crate::app::ConfirmExecutePipelineState {
                plan_ids,
                plan_titles,
                steps,
                pending_synthesis,
            });
        }
        PlansAction::Discuss => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st
                    .plans_view
                    .plans
                    .iter()
                    .find(|p| p.id == plan_id)
                    .cloned()
                {
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
                    VisiblePlanRow::ConceptItem { item_id, title, .. } => (
                        vec![item_id.clone()],
                        Some(format!("concept {} '{}'", item_id, title)),
                    ),
                    VisiblePlanRow::PlanItem { item_id, title, .. } => (
                        vec![item_id.clone()],
                        Some(format!("step {} '{}'", item_id, title)),
                    ),
                    _ => (Vec::new(), None),
                };
                let focus = tauqe_protocol::DiscussionFocus::plan(plan_id, focused);
                st.discussion_focus = focus.clone();
                let params = tauqe_protocol::DiscussionSetFocusParams { focus };
                let message = match target_label {
                    Some(label) => active_t("notify.plan_focus_item_set")
                        .replacen("{}", &label, 1)
                        .replace("{}", &plan_title),
                    None => active_t("notify.plan_focus_set").replace("{}", &plan_title),
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
                Some(VisiblePlanRow::PlanItem {
                    plan_id,
                    item_id,
                    title,
                    status,
                    ..
                }) => {
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
                    st.notify_info(active_t("notify.plan_concept_no_status"));
                }
                _ => {}
            }
        }
        PlansAction::DeletePlan => {
            let sel_row = st.plans_view.selected_row();
            if let Some(row) = sel_row {
                // Only the first tier is armed here: the second tier is raised
                // after this confirmation reveals unfinished work, so a stale
                // target of a previously selected plan can never resurface (c-1).
                st.confirm_delete_plan_final = None;
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
                            st.notify_success(active_t("notify.plan_copied"));
                        }
                        crate::clipboard::CopyResult::Osc52Only => {
                            st.notify_info(active_t("notify.plan_copied_osc52"));
                        }
                        crate::clipboard::CopyResult::Failed => {
                            st.notify_error(active_t("notify.plan_copy_failed"));
                        }
                    }
                }
            }
        }
        PlansAction::ClearHorizon => {
            let Some(row) = st.plans_view.selected_row() else {
                return Ok(InputResult::Continue);
            };
            let plan_id = row.plan_id().to_string();
            if !st.plans_view.plans.iter().any(|p| p.id == plan_id) {
                st.notify_error(active_t("notify.plan_not_found").replace("{}", &plan_id));
                return Ok(InputResult::Continue);
            }
            let horizon_empty = st
                .plans_view
                .plans
                .iter()
                .find(|p| p.id == plan_id)
                .map(|p| p.items.is_empty())
                .unwrap_or(false);
            // An empty horizon has nothing to clear, so the confirmation is
            // skipped and the developer is told why (Conventions §4.4).
            if horizon_empty {
                st.notify_info(active_t("notify.plan_horizon_already_empty"));
                return Ok(InputResult::Continue);
            }
            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            st.confirm_clear_horizon = Some(plan_id);
        }
        PlansAction::Replan => {
            if st.model.is_busy() || st.review.running || st.plans_view.refining {
                st.notify_warning(active_t("notify.model_busy"));
                return Ok(InputResult::Continue);
            }
            if let Some(row) = st.plans_view.selected_row() {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st
                    .plans_view
                    .plans
                    .iter()
                    .find(|p| p.id == plan_id)
                    .cloned()
                {
                    // A blocked step under the cursor seeds the instruction with
                    // its own context so a single `r` replans the horizon around
                    // exactly that failure (c-3).
                    let seed = match &row {
                        VisiblePlanRow::PlanItem {
                            item_id,
                            title,
                            status: tauqe_protocol::PlanItemStatus::Blocked,
                            ..
                        } => Some(format!(
                            "Replan uncompleted horizon around blocked step #{}: {}",
                            item_id, title
                        )),
                        _ => None,
                    };
                    open_refine_dialog(&mut st, plan, seed);
                }
            }
        }
        PlansAction::ReplanAll => {
            if st.model.is_busy() || st.review.running || st.plans_view.refining {
                st.notify_warning(active_t("notify.model_busy"));
                return Ok(InputResult::Continue);
            }
            if let Some(row) = st.plans_view.selected_row() {
                let plan_id = row.plan_id().to_string();
                if let Some(plan) = st
                    .plans_view
                    .plans
                    .iter()
                    .find(|p| p.id == plan_id)
                    .cloned()
                {
                    // `R` addresses the whole blocked frontier of the plan at
                    // once, seeding the instruction with every blocked step the
                    // scope reports (c-1, c-3).
                    let seed = match plan.evaluate_scope(None) {
                        tauqe_protocol::PlanScopeEvaluation::BlockedByBlockers(items) => {
                            Some(format!(
                                "Replan the uncompleted horizon addressing every blocked step:\n{}",
                                items.join("\n")
                            ))
                        }
                        _ => Some("Replan the uncompleted horizon of this plan".to_string()),
                    };
                    open_refine_dialog(&mut st, plan, seed);
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

    fn blocked_plan_state() -> crate::app::AppState {
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
        st
    }

    #[tokio::test]
    async fn test_lowercase_r_on_blocked_step_seeds_replan_instructions() {
        let state = Arc::new(Mutex::new(blocked_plan_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let res = handle_plans_key(
            KeyEvent::new(KeyCode::Char('r'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        assert!(matches!(res, InputResult::Continue));
        let guard = state.lock().await;
        let dialog = guard
            .plans_view
            .refine_dialog
            .as_ref()
            .expect("dialog opened");
        assert!(dialog
            .instructions_editor
            .get_text()
            .contains("Replan uncompleted horizon around blocked step #1: Failing step"));
        assert!(dialog.focus.is_instructions_editor());
    }

    #[tokio::test]
    async fn test_uppercase_r_seeds_all_blocked_steps() {
        let state = Arc::new(Mutex::new(blocked_plan_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        let dialog = guard
            .plans_view
            .refine_dialog
            .as_ref()
            .expect("dialog opened");
        let text = dialog.instructions_editor.get_text();
        assert!(
            text.contains("addressing every blocked step"),
            "seed: {text}"
        );
        assert!(text.contains("#1 Failing step"), "seed: {text}");
    }

    #[tokio::test]
    async fn test_ctrl_l_recenters_plans() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Plans;
        st.plans_view.plans = (1..=15)
            .map(|i| tauqe_protocol::Plan {
                id: format!("plan-{i}"),
                title: format!("Plan {i}"),
                ..Default::default()
            })
            .collect();
        st.plans_view.selected_index = 8;
        st.plans_view.view_height = 5;
        st.plans_view.content_width = 80;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let key = KeyEvent::new(KeyCode::Char('l'), crossterm::event::KeyModifiers::CONTROL);
        handle_plans_key(key, &state, &mut conn).await.unwrap();

        let scroll_first = state.lock().await.plans_view.scroll;
        handle_plans_key(key, &state, &mut conn).await.unwrap();
        let scroll_second = state.lock().await.plans_view.scroll;

        assert_ne!(scroll_first, scroll_second, "C-l must cycle recenter positions in plans view");
    }

    #[tokio::test]
    async fn test_g_refreshes_plans_and_r_no_longer_refreshes() {
        let state = Arc::new(Mutex::new(blocked_plan_state()));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // `g` triggers a lightweight PLAN_LIST refresh (no dialog opens).
        let res = handle_plans_key(
            KeyEvent::new(KeyCode::Char('g'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();
        assert!(matches!(res, InputResult::Continue));
        assert!(state.lock().await.plans_view.refine_dialog.is_none());
    }

    #[tokio::test]
    async fn test_uppercase_x_opens_whole_plan_pipeline_confirmation() {
        let mut st = crate::app::test_state();
        let plan = tauqe_protocol::Plan {
            id: "plan-ready".to_string(),
            title: "Ready Plan".to_string(),
            items: vec![
                tauqe_protocol::PlanItem {
                    id: "1".to_string(),
                    title: "First step".to_string(),
                    status: tauqe_protocol::PlanItemStatus::Todo,
                    ..Default::default()
                },
                tauqe_protocol::PlanItem {
                    id: "2".to_string(),
                    title: "Second step".to_string(),
                    status: tauqe_protocol::PlanItemStatus::Todo,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        st.plans_view.plans = vec![plan];
        st.plans_view.expand_all();

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('X'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_pipeline
            .as_ref()
            .expect("pipeline confirmation must be armed");
        assert_eq!(target.plan_ids, vec!["plan-ready".to_string()]);
        assert_eq!(target.plan_titles, vec!["Ready Plan".to_string()]);
        assert_eq!(target.steps.len(), 2);
        assert!(target.pending_synthesis.is_empty());
        assert!(guard.confirm_execute_scope.is_none());
    }

    #[tokio::test]
    async fn test_uppercase_x_queues_concept_only_plan_for_jit_synthesis() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![
            tauqe_protocol::Plan {
                id: "plan-concept".to_string(),
                title: "Concept Plan".to_string(),
                concept: vec![tauqe_protocol::ConceptItem {
                    id: "c1".to_string(),
                    title: "Invariant".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            tauqe_protocol::Plan {
                id: "plan-ready".to_string(),
                title: "Ready Plan".to_string(),
                items: vec![tauqe_protocol::PlanItem {
                    id: "1".to_string(),
                    title: "First step".to_string(),
                    status: tauqe_protocol::PlanItemStatus::Todo,
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
        st.plans_view.expand_all();

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('X'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_pipeline
            .as_ref()
            .expect("pipeline confirmation must be armed");
        assert_eq!(
            target.plan_ids,
            vec!["plan-concept".to_string(), "plan-ready".to_string()]
        );
        assert_eq!(target.steps.len(), 1);
        assert_eq!(target.pending_synthesis, vec!["Concept Plan".to_string()]);
    }

    #[tokio::test]
    async fn test_executing_concept_only_plan_arms_jit_confirmation() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-concept".to_string(),
            title: "Concept Plan".to_string(),
            concept: vec![tauqe_protocol::ConceptItem {
                id: "c1".to_string(),
                title: "Invariant".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }];
        st.plans_view.expand_all();

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('x'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_scope
            .as_ref()
            .expect("just-in-time confirmation must be armed");
        assert_eq!(target.plan_id, "plan-concept");
        assert!(target.step_id.is_none());
        assert!(target.steps.is_empty());
    }

    #[tokio::test]
    async fn test_clear_horizon_arms_level_one_when_steps_exist() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-steps".to_string(),
            title: "Steps Plan".to_string(),
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Todo step".to_string(),
                status: tauqe_protocol::PlanItemStatus::Todo,
                ..Default::default()
            }],
            ..Default::default()
        }];
        st.plans_view.expand_all();

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('C'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        assert_eq!(guard.confirm_clear_horizon.as_deref(), Some("plan-steps"));
        assert_eq!(
            guard.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
    }

    #[tokio::test]
    async fn test_clear_horizon_on_empty_plan_notifies_without_arming() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-empty".to_string(),
            title: "Empty Plan".to_string(),
            ..Default::default()
        }];
        st.plans_view.expand_all();

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_plans_key(
            KeyEvent::new(KeyCode::Char('C'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();

        let guard = state.lock().await;
        assert!(guard.confirm_clear_horizon.is_none());
        let notif = guard.active_notification().expect("notification");
        assert!(notif.text.contains("no implementation steps"));
    }

    /// Resolves a raw key event against the Plans command map with structural
    /// navigation active (text input inactive).
    fn resolve_plans(
        code: KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<PlansAction> {
        PLANS_COMMAND_MAP
            .resolve(
                KeyEvent::new(code, modifiers),
                &crate::config::InputConfig::default(),
                false,
            )
            .map(|spec| spec.action)
    }

    /// Proves `Shift+C` is canonized as the destructive `ClearHorizon` and is
    /// never intercepted by the copy binding, under every terminal spelling of a
    /// shifted letter (Conventions §4.4, c-1, c-3).
    #[test]
    fn shift_c_is_canonized_as_clear_horizon_not_copy() {
        use crossterm::event::KeyModifiers;

        // Uppercase `C` alone (the canonical `Shift+C` spelling on most
        // terminals), `C` carrying `SHIFT`, and the lowercase `c` carrying
        // `SHIFT` all open the destructive clear-horizon confirmation.
        assert_eq!(
            resolve_plans(KeyCode::Char('C'), KeyModifiers::NONE),
            Some(PlansAction::ClearHorizon)
        );
        assert_eq!(
            resolve_plans(KeyCode::Char('C'), KeyModifiers::SHIFT),
            Some(PlansAction::ClearHorizon)
        );
        assert_eq!(
            resolve_plans(KeyCode::Char('c'), KeyModifiers::SHIFT),
            Some(PlansAction::ClearHorizon)
        );

        // Lowercase `c` and both `y` cases stay copy, so the clipboard is never
        // bound to the destructive reset.
        assert_eq!(
            resolve_plans(KeyCode::Char('c'), KeyModifiers::NONE),
            Some(PlansAction::CopyMarkdown)
        );
        assert_eq!(
            resolve_plans(KeyCode::Char('y'), KeyModifiers::NONE),
            Some(PlansAction::CopyMarkdown)
        );
        assert_eq!(
            resolve_plans(KeyCode::Char('Y'), KeyModifiers::NONE),
            Some(PlansAction::CopyMarkdown)
        );
    }

    /// Proves that the copy and clear chords resolve identically when declared
    /// in the opposite order, so the destructive `Shift+C` is never shadowed by
    /// a positional declaration of copy (c-1, c-3).
    #[test]
    fn clear_horizon_and_copy_resolution_is_order_independent() {
        use crossterm::event::KeyModifiers;

        static REVERSED: &[CommandSpec<PlansAction>] = &[
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('C')),
                PlansAction::ClearHorizon,
                Helpers,
                "cmd.plans.clear_horizon",
            ),
            CommandSpec::new(
                KeyChord::Shift(KeyCode::Char('c')),
                PlansAction::ClearHorizon,
                Helpers,
                "cmd.plans.clear_horizon",
            ),
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('Y')),
                PlansAction::CopyMarkdown,
                Helpers,
                "cmd.plans.copy_markdown",
            ),
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('y')),
                PlansAction::CopyMarkdown,
                Helpers,
                "cmd.plans.copy_markdown",
            ),
            CommandSpec::new(
                KeyChord::Bare(KeyCode::Char('c')),
                PlansAction::CopyMarkdown,
                Helpers,
                "cmd.plans.copy_markdown",
            ),
        ];

        let map = CommandMap::new(REVERSED);
        let input = crate::config::InputConfig::default();
        let resolve = |code, modifiers| {
            map.resolve(KeyEvent::new(code, modifiers), &input, false)
                .map(|spec| spec.action)
        };

        assert_eq!(
            resolve(KeyCode::Char('C'), KeyModifiers::NONE),
            Some(PlansAction::ClearHorizon)
        );
        assert_eq!(
            resolve(KeyCode::Char('c'), KeyModifiers::SHIFT),
            Some(PlansAction::ClearHorizon)
        );
        assert_eq!(
            resolve(KeyCode::Char('c'), KeyModifiers::NONE),
            Some(PlansAction::CopyMarkdown)
        );
    }
}
