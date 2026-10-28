//! Mouse event routing for the non-modal surfaces.
//!
//! This module owns the pointer targets that are not dialogs: the header action
//! tabs, the Develop and History viewport scrolling, and the inline code-block
//! copy buttons. Modal interaction is delegated to the [`modal`] submodule, so a
//! dialog's click and wheel behaviour lives next to the geometry that draws it
//! instead of in one shared match.

use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::{Mutex, MutexGuard};

use crate::app::{AppState, ViewMode};
use crate::input::InputResult;
use crate::rpc::RpcConnection;
use crate::ui::UiAction;

pub mod modal;

pub use modal::{check_confirm_hit, ModalHit};

pub const MOUSE_SCROLL_STEP: usize = 3;

pub fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
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
            if modal::handle_modal_scroll(&mut st, mouse, term_rect) {
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
            if modal::handle_modal_scroll(&mut st, mouse, term_rect) {
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
                    // The wheel is free scrolling like paging: it stops at the
                    // last screenful of the rendered document and never
                    // re-anchors the viewport on the cursor (c-1, c-2).
                    let max = st.review.max_scroll();
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
            // Route the click to the canonical top modal so the surface that
            // renders is always the surface that receives the click (single
            // source of truth).
            if st.active_modal().is_some() {
                return modal::handle_modal_click(mouse, term_rect, state, st, server_writer).await;
            }

            // Interactive controls declare their own rectangles while drawing,
            // so the dispatcher resolves the pointer coordinate against the flat
            // registry of the current frame and never knows which widget lives
            // where (c-1, c-2). A hidden or covered surface registers nothing, so
            // it physically cannot receive the click (c-3).
            if let Some(action) = st.hit_regions.action_at(mouse.column, mouse.row) {
                dispatch_ui_action(action, state, st, server_writer).await?;
                return Ok(InputResult::Continue);
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

/// Executes the typed intention a component declared together with the
/// rectangle it drew.
///
/// The dispatcher is the only place that maps a [`UiAction`] to a state
/// transition, and it holds no knowledge of coordinates, widgets, or view
/// geometry. A component that starts registering a new action therefore touches
/// its own renderer and this match, never the pointer routing (c-1, c-2).
async fn dispatch_ui_action(
    action: UiAction,
    state: &Arc<Mutex<AppState>>,
    mut st: MutexGuard<'_, AppState>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    match action {
        UiAction::SwitchView(target) => {
            // A tab switch may lazily request History entries or the review
            // list, so the guard is released before the helper is awaited: the
            // event loop keeps rendering and the disconnection banner can still
            // appear when the daemon stops draining its stdin (c-1).
            drop(st);
            crate::input::switch_view(state, target, server_writer).await?;
        }
        UiAction::OpenModelSelect => {
            if st.model.is_busy() {
                st.notify_warning(crate::i18n::active_t("notify.model_select_busy"));
            } else {
                st.open_model_dialog();
            }
        }
        UiAction::ToggleHelp => {
            st.show_help = !st.show_help;
        }
        // A copy report comes from the surface that rendered the block, so the
        // dispatcher resolves the action against the owning view and reports the
        // typed outcome; it never recomputes a column or a scroll offset (c-1).
        UiAction::CopyCodeBlock { item, block } => {
            let copied = match item {
                Some(item_id) => st.history_view.copy_code_block(item_id, block),
                None => st.model.copy_code_block(block),
            };
            if let Some((result, lines)) = copied {
                report_code_copy(&mut st, result, lines);
            }
        }
        UiAction::CodeBlockBody => {
            // The hint explains once why a click cannot select the code text.
            if !st.shift_tip_shown {
                st.notify_info(crate::i18n::active_t("notify.mouse_shift_tip"));
                st.shift_tip_shown = true;
            }
        }
    }
    Ok(())
}

/// Reports the outcome of a code-block copy with the shared localized wording.
///
/// Both surfaces own the blocks they render and report the same typed
/// [`crate::clipboard::CopyResult`], so the notification text is written once
/// and Develop and History can never phrase one outcome differently (c-1).
fn report_code_copy(st: &mut AppState, result: crate::clipboard::CopyResult, lines: usize) {
    match result {
        crate::clipboard::CopyResult::Native => st.notify_success(
            crate::i18n::active_t("notify.code_copied").replace("{}", &lines.to_string()),
        ),
        crate::clipboard::CopyResult::Osc52Only => st.notify_success(
            crate::i18n::active_t("notify.code_copied_osc52").replace("{}", &lines.to_string()),
        ),
        crate::clipboard::CopyResult::Failed => {
            st.notify_error(crate::i18n::active_t("notify.code_copy_failed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use tauqe_protocol::methods;

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
            last_replanned_step: None,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                blocked_reason: None,
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
                uuid: None,
                group: None,
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
        assert_eq!(st.help_tab, crate::app::HelpTab::CurrentMode);
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
        tauqe_protocol::ModelEntry::new(tauqe_protocol::ModelRef::openrouter(name), None)
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
            last_replanned_step: None,
            concept: vec![],
            items: vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                blocked_reason: None,
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
        handle_mouse_event(
            left_click(cancel_button.x + 1, cancel_button.y),
            &state,
            &mut conn,
        )
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

    /// Builds a state with an open model picker listing two configured primary
    /// rungs.
    fn state_with_selection_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.selection_dialog = Some(crate::app::SelectionDialogState::new(
            vec![
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("alpha-unique"),
                    None,
                ),
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("beta-unique"),
                    None,
                ),
            ],
            0,
        ));
        st
    }

    /// Rung count of the open picker, read without cloning the dialog.
    fn selection_item_count(st: &crate::app::AppState) -> usize {
        st.selection_dialog
            .as_ref()
            .map(|dialog| dialog.selector.len())
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
        handle_mouse_event(
            left_click(cancel_button.x + 2, cancel_button.y),
            &state,
            &mut conn,
        )
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
        let list_area =
            crate::dialogs::render::selection::selection_dialog_list_area(term, item_count);

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

    /// A click on a rendered code block body resolves through the registry to
    /// the one-time selection hint, so the viewport no longer needs an inline
    /// geometry check inside the pointer dispatcher (c-1, c-2).
    #[tokio::test]
    async fn code_block_body_click_shows_the_selection_hint_once() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Repl;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.shift_tip_shown = false;
        st.hit_regions
            .register(Rect::new(2, 5, 40, 3), UiAction::CodeBlockBody);

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(3, 6), &state, &mut conn)
            .await
            .unwrap();

        let guard = state.lock().await;
        assert!(
            guard.shift_tip_shown,
            "the hint is shown on the first click"
        );
        assert!(guard.notification.is_some());
    }

    /// A copy action whose block disappeared between the frame and the click is
    /// dropped silently: the view owns the lookup and reports nothing to copy.
    #[tokio::test]
    async fn a_stale_copy_action_copies_nothing() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Repl;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.hit_regions.register(
            Rect::new(2, 5, 10, 1),
            UiAction::CopyCodeBlock {
                item: None,
                block: 7,
            },
        );

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(3, 5), &state, &mut conn)
            .await
            .unwrap();

        let guard = state.lock().await;
        assert!(guard.notification.is_none());
        assert!(guard.model.copy_flash.is_none());
        assert!(conn.written().is_empty());
    }

    /// Builds a Repl state whose frame registry already holds the regions a
    /// component declared while drawing.
    fn state_with_regions(regions: &[(Rect, UiAction)]) -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Repl;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        for (rect, action) in regions {
            st.hit_regions.register(*rect, *action);
        }
        st
    }

    /// A click resolves through the frame registry to the typed intention a
    /// component declared, and the dispatcher performs the transition without
    /// knowing which widget owns the rectangle (c-1, c-2).
    #[tokio::test]
    async fn registered_switch_view_click_switches_the_active_tab() {
        let st = state_with_regions(&[(
            Rect::new(5, 1, 10, 1),
            UiAction::SwitchView(ViewMode::Context),
        )]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(6, 1), &state, &mut conn)
            .await
            .unwrap();

        assert_eq!(state.lock().await.view_mode, ViewMode::Context);
    }

    /// A click outside every registered region dispatches nothing: the lookup
    /// never falls back to the nearest region and never triggers a transition
    /// the developer did not point at (c-2, c-3).
    #[tokio::test]
    async fn a_click_outside_every_registered_region_dispatches_nothing() {
        let st = state_with_regions(&[(
            Rect::new(5, 1, 10, 1),
            UiAction::SwitchView(ViewMode::Plans),
        )]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(60, 20), &state, &mut conn)
            .await
            .unwrap();

        assert_eq!(state.lock().await.view_mode, ViewMode::Repl);
        assert!(conn.written().is_empty());
    }

    /// The later declaration wins over the surface it covers, so a button drawn
    /// on top of a tab receives the click instead of the tab underneath (c-1).
    #[tokio::test]
    async fn the_top_most_declaration_wins_over_the_surface_it_covers() {
        let st = state_with_regions(&[
            (
                Rect::new(0, 0, 40, 3),
                UiAction::SwitchView(ViewMode::Plans),
            ),
            (Rect::new(10, 0, 5, 1), UiAction::ToggleHelp),
        ]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(12, 0), &state, &mut conn)
            .await
            .unwrap();

        let guard = state.lock().await;
        assert!(guard.show_help);
        assert_eq!(guard.view_mode, ViewMode::Repl);
    }

    /// An active modal takes the click before the registry is consulted, so a
    /// region covered by a dialog cannot fire from under it (c-3).
    #[tokio::test]
    async fn an_active_modal_consumes_the_click_before_the_covered_regions() {
        let mut st = state_with_regions(&[(
            Rect::new(0, 0, 40, 3),
            UiAction::SwitchView(ViewMode::Plans),
        )]);
        st.open_usage_summary();
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(1, 1), &state, &mut conn)
            .await
            .unwrap();

        let guard = state.lock().await;
        assert_eq!(guard.view_mode, ViewMode::Repl);
        assert!(guard.usage_summary.is_none());
    }

    /// A cleared registry holds no region of the finished frame, so the same
    /// coordinate that used to dispatch resolves to nothing (c-3).
    #[tokio::test]
    async fn a_cleared_registry_dispatches_nothing() {
        let mut st = state_with_regions(&[(Rect::new(5, 1, 10, 1), UiAction::ToggleHelp)]);
        st.hit_regions.clear();
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(6, 1), &state, &mut conn)
            .await
            .unwrap();

        assert!(!state.lock().await.show_help);
    }

    /// The wheel is free scrolling too: a long expanded subplan scrolls
    /// continuously down to its last screenful and no further, while the cursor
    /// keeps its row (c-1, c-2).
    #[tokio::test]
    async fn review_wheel_scrolls_long_subplan_to_the_bottom_without_snapping() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.review.view_height = 20;
        st.review.rendered_lines = 500;
        st.review.selected_index = 0;
        st.review.scroll = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let down = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse_event(down, &state, &mut conn).await.unwrap();
        assert_eq!(
            state.lock().await.review.scroll,
            MOUSE_SCROLL_STEP as u16,
            "one notch moves the window by the shared scroll step"
        );

        // Notches past the end stop exactly at the last full screenful.
        for _ in 0..200 {
            handle_mouse_event(down, &state, &mut conn).await.unwrap();
        }
        let guard = state.lock().await;
        assert_eq!(guard.review.scroll, 480);
        assert_eq!(guard.review.selected_index, 0);
    }
}
