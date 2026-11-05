//! Mouse event routing for the non-modal surfaces.
//!
//! This module owns the pointer targets that are not dialogs: the header action
//! tabs, the REPL and History viewport scrolling, the inline code-block copy
//! buttons, and the hyperlinks a Markdown surface paints. Modal interaction is
//! delegated to the [`modal`] submodule, so a dialog's click and wheel behaviour
//! lives next to the geometry that draws it instead of in one shared match.

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
                    // The bound is computed by the view in `usize` and only
                    // then narrowed, so a history longer than the `u16`
                    // viewport unit saturates instead of wrapping through a
                    // truncated cast (c-2).
                    let max = st.history_view.max_scroll(view_height);
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
                    // The tree measures its own bound, so a tree longer than
                    // the `u16` viewport unit saturates instead of wrapping
                    // through a truncated cast (c-2).
                    let max = st.plans_view.max_scroll();
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
            // Middle-click pastes the terminal Primary Selection. The read is a
            // native round trip bounded at NATIVE_CLIPBOARD_TIMEOUT, so the state
            // guard is released before it and re-acquired only to deliver the
            // text. Routing the text through `handle_paste` keeps the modal stack
            // authoritative: only the visible, top-most surface ever receives
            // the paste (c-1).
            drop(st);
            let pasted = crate::clipboard::get_primary_or_clipboard_async().await;
            let mut st = state.lock().await;
            if let Some(text) = pasted {
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
            st.modals.show_help = !st.modals.show_help;
        }
        // A copy report comes from the surface that rendered the block, so the
        // dispatcher resolves the action against the owning view and reports the
        // typed outcome; it never recomputes a column or a scroll offset (c-1).
        // The clipboard round trip is awaited after the guard is released, so a
        // stalled display server can never freeze the event loop (c-1).
        UiAction::CopyCodeBlock { item, block } => {
            let source = match item {
                Some(item_id) => st.history_view.code_block_source(item_id, block),
                None => st.model.code_block_source(block),
            };
            if let Some(code) = source {
                let lines = code.lines().count().max(1);
                drop(st);
                let result = crate::clipboard::copy_to_clipboard_async(&code).await;
                let mut st = state.lock().await;
                if result.is_success() {
                    match item {
                        Some(item_id) => st.history_view.arm_code_copy_flash(item_id, block),
                        None => st.model.arm_code_copy_flash(block),
                    }
                }
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
        // Opening a link is a local interface side effect the dispatcher owns:
        // nothing about it reaches the daemon, and every decision it makes is
        // reported back to the developer (c-2, c-3, c-4).
        UiAction::OpenUrl(url) => {
            // The scheme gate runs before any process work, so a dangerous or
            // relative address never reaches an external launcher (c-2).
            if !is_openable_scheme(&url) {
                let template = crate::i18n::active_t("notify.unsafe_link_rejected");
                st.notify_warning(template.replacen("{}", &url, 1));
                return Ok(());
            }

            // Over SSH a browser would open on the remote host, where the
            // developer cannot see it, so the address goes to the clipboard
            // instead of an unreachable window (c-4).
            if crate::clipboard::is_ssh_session() {
                drop(st);
                let copied = crate::clipboard::copy_to_clipboard_async(&url).await;
                let mut st = state.lock().await;
                report_link_clipboard(&mut st, &url, copied, LinkFallback::SshSession);
                return Ok(());
            }

            // The launch runs on the blocking pool, so a slow launcher can never
            // stall the draw loop or the server drain (c-4).
            drop(st);
            if launch_in_browser(&url).await {
                let mut st = state.lock().await;
                let template = crate::i18n::active_t("notify.link_opened");
                st.notify_success(template.replacen("{}", &url, 1));
                return Ok(());
            }

            // A host without a graphical browser degrades to the clipboard
            // instead of failing silently (c-4).
            let copied = crate::clipboard::copy_to_clipboard_async(&url).await;
            let mut st = state.lock().await;
            report_link_clipboard(&mut st, &url, copied, LinkFallback::NoBrowser);
        }
    }
    Ok(())
}

/// Reports the outcome of a code-block copy with the shared localized wording.
///
/// Both surfaces own the blocks they render and report the same typed
/// [`crate::clipboard::CopyResult`], so the notification text is written once
/// and REPL and History can never phrase one outcome differently (c-1).
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
/// Why the launcher degraded to the clipboard.
enum LinkFallback {
    /// The session runs over SSH, where a browser opens on the remote host.
    SshSession,
    /// No browser could be launched on this host.
    NoBrowser,
}

/// Whether the launcher may hand `url` to an external browser.
///
/// An absolute URL starts with the RFC 3986 scheme production
/// `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` followed by a colon, and that
/// scheme decides whether the address is safe to open. Only the two web schemes
/// pass, so `javascript:`, `data:`, `file:`, and every relative path stay inside
/// the harness instead of reaching a process (c-2).
fn is_openable_scheme(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')) {
        return false;
    }
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

/// Hands the validated address to the developer's browser, off the draw loop.
///
/// The launch is blocking by nature: `open` spawns a detached child process. The
/// work therefore runs on the blocking pool while the caller awaits it, so the
/// interface keeps drawing and keeps draining the daemon (c-4). A host that
/// cannot spawn a browser reports `false`, which degrades to the clipboard.
async fn launch_in_browser(url: &str) -> bool {
    let address = url.to_string();
    tokio::task::spawn_blocking(move || open::that_detached(address).is_ok())
        .await
        .unwrap_or(false)
}

/// Reports the clipboard fallback of a link the launcher could not open.
///
/// Both fallbacks, an SSH session and a host without a browser, copy the address
/// and state why it was copied, so the developer knows where the address went
/// instead of a browser window (c-4, c-5).
fn report_link_clipboard(
    st: &mut AppState,
    url: &str,
    copied: crate::clipboard::CopyResult,
    fallback: LinkFallback,
) {
    if !copied.is_success() {
        let template = crate::i18n::active_t("notify.link_open_failed");
        st.notify_error(template.replacen("{}", url, 1));
        return;
    }
    let key = match fallback {
        LinkFallback::SshSession => "notify.link_copied_ssh",
        LinkFallback::NoBrowser => "notify.link_copied_no_browser",
    };
    let template = crate::i18n::active_t(key);
    st.notify_warning(template.replacen("{}", url, 1));
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
        let body: Vec<ratatui::text::Line> = vec![ratatui::text::Line::raw(""); 5];
        let geom = crate::dialogs::render::confirm_modal_geometry(
            term,
            58,
            &body,
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
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-1".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec![],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        )];
        st.modals.status_dialog = Some(crate::app::StatusDialogState::new(
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
        assert!(
            st.modals.status_dialog.is_none(),
            "the dialog must be consumed"
        );
        let item = st
            .plans_view
            .plans
            .iter()
            .find(|p| p.id == "plan-1")
            .and_then(|p| p.implementation.find_item("1"))
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
        st.modals.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "All remaining TODO findings".to_string(),
            Some(1),
            None,
            vec![tauqe_protocol::ReviewIssue {
                id: 1,
                title: "Finding".to_string(),
                uuid: None,
                commit: None,
                group: None,
                model: None,
                issue_type: tauqe_protocol::IssueType::Architecture,
                status: tauqe_protocol::IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                note: None,
            }],
            entries,
        ));

        let term = st.last_frame_area;
        let entry_count = st
            .modals
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
        assert!(st.modals.confirm_execute_review.is_none());
    }

    /// A click on a Help header tab must switch to that tab without closing the
    /// dialog.
    #[tokio::test]
    async fn help_tab_click_switches_tab_without_closing() {
        let mut st = crate::app::test_state();
        st.modals.show_help = true;
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
        assert!(
            st.modals.show_help,
            "clicking a tab must not close the dialog"
        );
        assert_eq!(st.modals.help_tab, crate::app::HelpTab::CurrentMode);
    }

    /// A click on the locale dialog's `Cancel` button must dismiss it.
    #[tokio::test]
    async fn locale_cancel_button_click_dismisses_dialog() {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.modals.locale_dialog = Some(crate::app::LocaleDialogState::default());
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
        assert!(st.modals.locale_dialog.is_none());
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
        assert!(state.lock().await.modals.usage_summary.is_some());

        handle_mouse_event(left_click(0, 0), &state, &mut conn)
            .await
            .unwrap();
        assert!(state.lock().await.modals.usage_summary.is_none());
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
            state
                .lock()
                .await
                .modals
                .usage_summary
                .as_ref()
                .unwrap()
                .scroll,
            MOUSE_SCROLL_STEP as u16
        );

        let up = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..down
        };
        handle_mouse_event(up, &state, &mut conn).await.unwrap();
        assert_eq!(
            state
                .lock()
                .await
                .modals
                .usage_summary
                .as_ref()
                .unwrap()
                .scroll,
            0
        );
    }

    fn left_click(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    /// Builds a Plans state holding one `Todo` step and an open status dialog,
    /// so a click test can assert both the applied status and the request.
    fn state_with_plan_status_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Plans;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-1".to_string(),
            "Test Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![tauqe_protocol::PlanItem {
                id: "1".to_string(),
                title: "Step".to_string(),
                details: None,
                status: tauqe_protocol::PlanItemStatus::Todo,
                blocked_reason: None,
                commit: None,
                files: vec![],
                readonly_files: vec![],
                outer_docs: vec![],
                concept_refs: vec![],
                depends_on: vec![],
                children: vec![],
            }],
        )];
        st.modals.status_dialog = Some(crate::app::StatusDialogState::new(
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
            .modals
            .status_dialog
            .as_ref()
            .map(|dialog| dialog.target.options_count())
            .unwrap_or(0);
        let options_area = crate::dialogs::render::status_dialog_options_area(term, options_count);

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|frame| {
                if let Some(dialog) = st.modals.status_dialog.as_ref() {
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
        assert!(
            guard.modals.status_dialog.is_none(),
            "the dialog must be consumed"
        );
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.implementation.find_item("1"))
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
            guard.modals.status_dialog.is_some(),
            "an air row keeps the dialog open"
        );
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.implementation.find_item("1"))
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
            .modals
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
        assert!(guard.modals.status_dialog.is_none());
        let item = guard
            .plans_view
            .plans
            .iter()
            .find(|plan| plan.id == "plan-1")
            .and_then(|plan| plan.implementation.find_item("1"))
            .expect("the plan step must exist");
        assert_eq!(item.status, tauqe_protocol::PlanItemStatus::Todo);
    }

    /// Builds a state with an open model picker listing two configured primary
    /// rungs.
    fn state_with_selection_dialog() -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.modals.selection_dialog = Some(crate::app::SelectionDialogState::new(
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
        st.modals
            .selection_dialog
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
        assert!(state.lock().await.modals.selection_dialog.is_none());
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
        assert!(state.lock().await.modals.selection_dialog.is_none());
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
        assert!(state.lock().await.modals.selection_dialog.is_none());
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
        assert!(state.lock().await.modals.selection_dialog.is_some());
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
                if let Some(dialog) = st.modals.selection_dialog.as_ref() {
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

    /// A click on a rendered code-block copy button resolves the block through
    /// the owning view, awaits the clipboard round trip outside the state guard,
    /// and re-locks only to report the typed outcome (c-1, c-2).
    #[tokio::test]
    async fn copy_code_block_click_reports_the_typed_outcome() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Repl;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        st.history_view
            .code_blocks
            .push(crate::views::history::HistoryCodeBlock {
                item_id: 1,
                block_id: 0,
                lang: "rust".to_string(),
                code: "fn main() {}".to_string(),
                visual_start_line: 0,
                visual_end_line: 0,
            });
        st.hit_regions.register(
            Rect::new(2, 5, 10, 1),
            UiAction::CopyCodeBlock {
                item: Some(1),
                block: 0,
            },
        );

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        handle_mouse_event(left_click(3, 5), &state, &mut conn)
            .await
            .unwrap();

        let guard = state.lock().await;
        assert!(
            guard.notification.is_some(),
            "the copy reports its typed outcome instead of succeeding silently"
        );
        assert!(
            conn.written().is_empty(),
            "a copy is a local action and never reaches the daemon"
        );
    }

    /// Builds a Repl state whose frame registry already holds the regions a
    /// component declared while drawing.
    fn state_with_regions(regions: &[(Rect, UiAction)]) -> crate::app::AppState {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Repl;
        st.last_frame_area = Rect::new(0, 0, 100, 30);
        for (rect, action) in regions {
            st.hit_regions.register(*rect, action.clone());
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
        assert!(guard.modals.show_help);
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
        assert!(guard.modals.usage_summary.is_none());
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

        assert!(!state.lock().await.modals.show_help);
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
    /// A hyperlink region resolves through the registry to the address it
    /// declares, and the dispatcher handles it locally: opening a link never
    /// reaches the daemon, and a scheme the launcher refuses is reported instead
    /// of being handed to an external process (c-2, c-3).
    #[tokio::test]
    async fn a_registered_link_click_dispatches_without_a_request() {
        let st = state_with_regions(&[(
            Rect::new(2, 5, 20, 1),
            UiAction::OpenUrl("file:///etc/passwd".to_string()),
        )]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(3, 5), &state, &mut conn)
            .await
            .unwrap();

        assert!(conn.written().is_empty());
        let guard = state.lock().await;
        assert_eq!(guard.view_mode, ViewMode::Repl);
        assert!(
            guard.notification.is_some(),
            "a refused scheme is reported instead of launched"
        );
    }

    /// The scheme gate accepts exactly the two web schemes, in either case, and
    /// refuses every other protocol together with every relative address, so a
    /// clicked link can never smuggle a local path or a script into an external
    /// launcher (c-2).
    #[test]
    fn the_scheme_gate_accepts_only_web_addresses() {
        for openable in [
            "http://example.com",
            "https://example.com/path?query=1#fragment",
            "HTTPS://EXAMPLE.COM",
            "Http://example.com",
        ] {
            assert!(is_openable_scheme(openable), "{openable} must be openable");
        }

        for refused in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "ftp://example.com",
            "mailto:dev@example.com",
            "tel:+10000000000",
            "docs/readme.md",
            "example.com",
            "#anchor",
            "",
            ":no-scheme",
            "ht tp://example.com",
            "C:/Windows/system.ini",
        ] {
            assert!(!is_openable_scheme(refused), "{refused} must be refused");
        }
    }
    /// A refused scheme reaches no external process and is reported with the
    /// catalog wording that names the address, so the developer learns why the
    /// click did nothing (c-2, c-5).
    #[tokio::test]
    async fn a_refused_scheme_reports_the_localized_wording() {
        let address = "file:///etc/passwd";
        let st = state_with_regions(&[(
            Rect::new(2, 5, 20, 1),
            UiAction::OpenUrl(address.to_string()),
        )]);
        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        handle_mouse_event(left_click(3, 5), &state, &mut conn)
            .await
            .unwrap();

        assert!(
            conn.written().is_empty(),
            "a refused link never reaches the daemon"
        );
        let guard = state.lock().await;
        let notification = guard
            .notification
            .as_ref()
            .expect("a refused scheme is reported");
        assert_eq!(
            notification.text,
            crate::i18n::active_t("notify.unsafe_link_rejected").replacen("{}", address, 1)
        );
        assert!(matches!(
            notification.level,
            crate::app::NotificationLevel::Warning
        ));
    }

    /// The SSH fallback copies the address instead of opening a browser the
    /// developer could not see, worded by the catalog (c-4, c-5).
    #[test]
    fn the_ssh_fallback_reports_the_catalog_wording() {
        let address = "https://example.com/docs";
        let mut st = crate::app::test_state();
        report_link_clipboard(
            &mut st,
            address,
            crate::clipboard::CopyResult::Native,
            LinkFallback::SshSession,
        );

        let notification = st.notification.as_ref().expect("the fallback is reported");
        assert_eq!(
            notification.text,
            crate::i18n::active_t("notify.link_copied_ssh").replacen("{}", address, 1)
        );
        assert!(matches!(
            notification.level,
            crate::app::NotificationLevel::Warning
        ));
    }

    /// A host without a graphical browser degrades the same way and states its
    /// own reason through its own entry (c-4, c-5).
    #[test]
    fn the_no_browser_fallback_reports_the_catalog_wording() {
        let address = "https://example.com/docs";
        let mut st = crate::app::test_state();
        report_link_clipboard(
            &mut st,
            address,
            crate::clipboard::CopyResult::Osc52Only,
            LinkFallback::NoBrowser,
        );

        let notification = st.notification.as_ref().expect("the fallback is reported");
        assert_eq!(
            notification.text,
            crate::i18n::active_t("notify.link_copied_no_browser").replacen("{}", address, 1)
        );
    }

    /// When neither the browser nor the clipboard is reachable the dispatcher
    /// reports the open failure as an error that names the address (c-4, c-5).
    #[test]
    fn an_unreachable_clipboard_reports_the_open_failure() {
        let address = "https://example.com/docs";
        let mut st = crate::app::test_state();
        report_link_clipboard(
            &mut st,
            address,
            crate::clipboard::CopyResult::Failed,
            LinkFallback::NoBrowser,
        );

        let notification = st.notification.as_ref().expect("the failure is reported");
        assert_eq!(
            notification.text,
            crate::i18n::active_t("notify.link_open_failed").replacen("{}", address, 1)
        );
        assert!(matches!(
            notification.level,
            crate::app::NotificationLevel::Error
        ));
    }
}
