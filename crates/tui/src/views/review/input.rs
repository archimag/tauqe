use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::{
    methods, IssueStatus, ModelRole, ModelSelection, ReviewIssue, ReviewRefineItemParams,
};
use tokio::sync::Mutex;

use crate::app::{AppState, ReviewDialogState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::i18n::engine::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::{RefineTarget, ReviewRefineDialogState, VisibleReviewRow};

/// Keyboard actions available in the Review view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    Return,
    Activate,
    Execute,
    ExecuteAll,
    RunReview,
    Discuss,
    Refine,
    RefineAll,
    ChangeStatus,
    SetFocus,
    ToggleHideClosed,
    NavPrev,
    NavNext,
    AccordionPrev,
    AccordionNext,
    SelectFirst,
    SelectLast,
    Recenter,
    PageUp,
    PageDown,
    FoldToggle,
    FoldAll,
    Copy,
    Refresh,
    DeleteReview,
}

/// Single source of truth for the Review view keybindings.
///
/// Essential commands drive finding execution and the review cycle; Interface
/// commands navigate, fold, and filter the tree; Helpers copy, refresh, and
/// delete. The map drives the generated Help dialog. Case-differentiated chords
/// are declared first so a shifted lowercase key always resolves to its explicit
/// uppercase binding before the bare binding of the same letter.
pub static REVIEW_COMMAND_MAP: CommandMap<ReviewAction> = CommandMap::new(&[
    // Case-differentiated execution chords, declared before the bare `x`.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('X')),
        ReviewAction::ExecuteAll,
        Essential,
        "cmd.review.execute_all",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('x')),
        ReviewAction::ExecuteAll,
        Essential,
        "cmd.review.execute_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('x')),
        ReviewAction::Execute,
        Essential,
        "cmd.review.execute",
    ),
    // Case-differentiated decomposition chords, declared before the bare `r`.
    // Lowercase `r` decomposes the selection, uppercase `R` decomposes every
    // finding that still awaits decomposition (Conventions §4.5).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('R')),
        ReviewAction::RefineAll,
        Essential,
        "cmd.review.refine_all",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('r')),
        ReviewAction::RefineAll,
        Essential,
        "cmd.review.refine_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('r')),
        ReviewAction::Refine,
        Essential,
        "cmd.review.refine",
    ),
    // Starting a new review session is a creation verb, so it takes `N`; `r` and
    // `R` stay reserved for the Architect and refresh keeps the canonical `g`
    // (Conventions §4.5, §4.6).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('N')),
        ReviewAction::RunReview,
        Essential,
        "cmd.review.run",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('n')),
        ReviewAction::RunReview,
        Essential,
        "cmd.review.run",
    ),
    // Case-differentiated filter chords, declared before the bare `f`.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('F')),
        ReviewAction::ToggleHideClosed,
        Interface,
        "cmd.review.toggle_hide_closed",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('f')),
        ReviewAction::ToggleHideClosed,
        Interface,
        "cmd.review.toggle_hide_closed",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('f')),
        ReviewAction::SetFocus,
        Interface,
        "cmd.review.set_focus",
    ),
    // Essential: review execution and the workflow cycle.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Enter),
        ReviewAction::Activate,
        Essential,
        "cmd.review.activate",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('d')),
        ReviewAction::Discuss,
        Essential,
        "cmd.review.discuss",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('D')),
        ReviewAction::Discuss,
        Essential,
        "cmd.review.discuss",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('t')),
        ReviewAction::ChangeStatus,
        Essential,
        "cmd.review.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('T')),
        ReviewAction::ChangeStatus,
        Essential,
        "cmd.review.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('s')),
        ReviewAction::ChangeStatus,
        Essential,
        "cmd.review.change_status",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('S')),
        ReviewAction::ChangeStatus,
        Essential,
        "cmd.review.change_status",
    ),
    // Interface: navigation, accordion walk, folding, and scrolling.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Esc),
        ReviewAction::Return,
        Interface,
        "cmd.review.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('q')),
        ReviewAction::Return,
        Interface,
        "cmd.review.return",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Up),
        ReviewAction::NavPrev,
        Interface,
        "cmd.review.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('p')),
        ReviewAction::NavPrev,
        Interface,
        "cmd.review.nav_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Down),
        ReviewAction::NavNext,
        Interface,
        "cmd.review.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('n')),
        ReviewAction::NavNext,
        Interface,
        "cmd.review.nav_next",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('n')),
        ReviewAction::AccordionNext,
        Interface,
        "cmd.review.accordion_next",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('n')),
        ReviewAction::AccordionNext,
        Interface,
        "cmd.review.accordion_next",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('p')),
        ReviewAction::AccordionPrev,
        Interface,
        "cmd.review.accordion_prev",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('p')),
        ReviewAction::AccordionPrev,
        Interface,
        "cmd.review.accordion_prev",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Home),
        ReviewAction::SelectFirst,
        Interface,
        "cmd.review.select_first",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('<')),
        ReviewAction::SelectFirst,
        Interface,
        "cmd.review.select_first",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Home),
        ReviewAction::SelectFirst,
        Interface,
        "cmd.review.select_first",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::End),
        ReviewAction::SelectLast,
        Interface,
        "cmd.review.select_last",
    ),
    CommandSpec::new(
        KeyChord::Meta(KeyCode::Char('>')),
        ReviewAction::SelectLast,
        Interface,
        "cmd.review.select_last",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::End),
        ReviewAction::SelectLast,
        Interface,
        "cmd.review.select_last",
    ),
    CommandSpec::new(
        KeyChord::Primary(KeyCode::Char('l')),
        ReviewAction::Recenter,
        Interface,
        "cmd.review.recenter",
    ),
    CommandSpec::new(
        KeyChord::Ctrl(KeyCode::Char('l')),
        ReviewAction::Recenter,
        Interface,
        "cmd.review.recenter",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageUp),
        ReviewAction::PageUp,
        Interface,
        "cmd.review.page_up",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::PageDown),
        ReviewAction::PageDown,
        Interface,
        "cmd.review.page_down",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Tab),
        ReviewAction::FoldToggle,
        Interface,
        "cmd.review.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char(' ')),
        ReviewAction::FoldToggle,
        Interface,
        "cmd.review.fold_toggle",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('a')),
        ReviewAction::FoldAll,
        Interface,
        "cmd.review.fold_all",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('A')),
        ReviewAction::FoldAll,
        Interface,
        "cmd.review.fold_all",
    ),
    // Helpers: clipboard, refresh, and destructive operations.
    //
    // The clipboard is isolated from destructive clearing: lowercase `c` and `y`
    // only place content into the clipboard, and the shifted `Shift+C` chord is
    // deliberately left unbound here so Plans, Review, History, and Context all
    // reserve the uppercase chord for a confirmed mass clear (Conventions §4.4).
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('c')),
        ReviewAction::Copy,
        Helpers,
        "cmd.review.copy",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('y')),
        ReviewAction::Copy,
        Helpers,
        "cmd.review.copy",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('Y')),
        ReviewAction::Copy,
        Helpers,
        "cmd.review.copy",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Delete),
        ReviewAction::DeleteReview,
        Helpers,
        "cmd.review.delete",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('g')),
        ReviewAction::Refresh,
        Helpers,
        "cmd.review.refresh",
    ),
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('G')),
        ReviewAction::Refresh,
        Helpers,
        "cmd.review.refresh",
    ),
]);

/// Arms the execution confirmation for a finding, a subplan step, or a session
/// queue.
///
/// The confirmation state is built through the single constructor shared with
/// the mouse path, so point execution (one finding or one step) always carries
/// the Coder selector and the directives editor while a queue runs on the
/// configured role defaults (concept #c-3).
fn execute_review_scope_action(
    st: &mut AppState,
    session_id: &str,
    target_item_id: Option<u32>,
    target_step_id: Option<String>,
    scope_title: &str,
) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let session = match st
        .review
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .cloned()
        .or_else(|| {
            st.review
                .session
                .as_ref()
                .filter(|s| s.id == session_id)
                .cloned()
        }) {
        Some(s) => s,
        None => {
            st.notify_error(active_t("notify.review_not_found").replace("{}", session_id));
            return;
        }
    };

    let session_title = if session.title.trim().is_empty() {
        session.id.clone()
    } else {
        session.title.clone()
    };

    if let Some(item_id) = target_item_id {
        let item = match session.find_item(item_id) {
            Some(i) => i.clone(),
            None => {
                st.notify_error(
                    active_t("notify.review_item_not_found").replace("{}", &item_id.to_string()),
                );
                return;
            }
        };

        // Every status that can still produce work is dispatched: a `REFINE`
        // finding is decomposed by the Architect before the first Coder call on
        // the server, so the client never blocks execution on a missing subplan
        // (#c-1).
        match item.status {
            IssueStatus::Discussion => {
                st.notify_warning(
                    active_t("notify.review_discussion_status").replace("{}", &item.id.to_string()),
                );
            }
            IssueStatus::Fixed => {
                st.notify_info(
                    active_t("notify.review_already_fixed").replace("{}", &item.id.to_string()),
                );
            }
            IssueStatus::Skipped => {
                st.notify_info(
                    active_t("notify.review_skipped_status").replace("{}", &item.id.to_string()),
                );
            }
            IssueStatus::Rejected => {
                st.notify_warning(
                    active_t("notify.review_rejected_status").replace("{}", &item.id.to_string()),
                );
            }
            // A `REFINE` finding runs exactly like a planned one: the server
            // decomposes it with the Architect before the first Coder call, so a
            // missing subplan never blocks execution (#c-1).
            IssueStatus::Refine | IssueStatus::Todo | IssueStatus::InProgress => {
                // Point execution opens the Coder form so the implementing model
                // and the directives are chosen before a bounded turn runs
                // (concept #c-3).
                let coder_entries = st.coder_entries();
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
                    Some(session.id.clone()),
                    session_title,
                    scope_title.to_string(),
                    Some(item.id),
                    target_step_id.clone(),
                    vec![ReviewIssue::from(item)],
                    coder_entries,
                ));
            }
        }
    } else {
        // A finding still awaiting architectural decomposition belongs to the
        // queue too: the server refines it before running its steps (#c-1).
        let todo_items: Vec<ReviewIssue> = session
            .items
            .iter()
            .filter(|i| {
                matches!(
                    i.status,
                    IssueStatus::Refine | IssueStatus::Todo | IssueStatus::InProgress
                )
            })
            .cloned()
            .map(ReviewIssue::from)
            .collect();

        if todo_items.is_empty() {
            let (total, fixed) = session.stats();
            if fixed == total && total > 0 {
                st.notify_info(active_t("notify.review_all_fixed_session"));
            } else {
                st.notify_info(active_t("notify.review_no_todo_session"));
            }
            return;
        }

        // A session-wide queue is a batch: it runs autonomously on the configured
        // Coder defaults, so no model selector is exposed (concept #c-3).
        let coder_entries = st.coder_entries();
        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
        st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
            Some(session.id.clone()),
            session_title,
            format!("All remaining TODO findings ({} items)", todo_items.len()),
            None,
            None,
            todo_items,
            coder_entries,
        ));
    }
}

pub(crate) fn execute_all_reviews_action(st: &mut AppState) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let sessions = st.review.effective_sessions();
    if sessions.is_empty() {
        st.notify_info(active_t("notify.review_no_sessions"));
        return;
    }

    let mut todo_items: Vec<ReviewIssue> = Vec::new();
    let mut total_items = 0;
    let mut discussion_count = 0;
    let mut fixed_count = 0;

    for session in &sessions {
        for item in &session.items {
            total_items += 1;
            match item.status {
                // A finding that still awaits architectural decomposition is
                // executable: the server refines it with the Architect before
                // the first Coder call (#c-1), exactly as single-session batch
                // execution does. Omitting it here silently skipped work under
                // the cross-session `X` verb.
                IssueStatus::Refine | IssueStatus::Todo | IssueStatus::InProgress => {
                    todo_items.push(ReviewIssue::from(item.clone()));
                }
                IssueStatus::Discussion => {
                    discussion_count += 1;
                }
                IssueStatus::Fixed => {
                    fixed_count += 1;
                }
                _ => {}
            }
        }
    }

    if todo_items.is_empty() {
        if total_items == 0 {
            st.notify_info(active_t("notify.review_no_findings"));
        } else if discussion_count > 0 {
            st.notify_warning(active_t("notify.review_discussion_only"));
        } else if fixed_count == total_items {
            st.notify_info(active_t("notify.review_all_fixed"));
        } else {
            st.notify_info(active_t("notify.review_no_todo_cross"));
        }
        return;
    }

    let coder_entries = st.coder_entries();
    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
    st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
        None,
        "All Review Sessions".to_string(),
        format!(
            "All remaining TODO findings across all sessions ({} items)",
            todo_items.len()
        ),
        None,
        None,
        todo_items,
        coder_entries,
    ));
}

/// Title of the finding that owns a subplan step.
///
/// Executing a step runs the owning finding's bounded turn, so the confirmation
/// dialog must name the finding instead of the individual step.
fn finding_scope_title(st: &AppState, session_id: &str, item_id: u32) -> String {
    st.review
        .sessions
        .iter()
        .chain(st.review.session.iter())
        .find(|session| session.id == session_id)
        .and_then(|session| session.items.iter().find(|item| item.id == item_id))
        .map(|item| item.title.clone())
        .unwrap_or_else(|| format!("Finding #{}", item_id))
}

fn format_item_for_clipboard(issue: &ReviewIssue) -> String {
    let mut text = format!("[{}] {}", issue.issue_type, issue.title);
    if !issue.files.is_empty() {
        text.push('\n');
        text.push_str(&issue.files.join(", "));
    }
    if let Some(note) = issue.note.as_deref() {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            text.push_str("\n\nResolution: ");
            text.push_str(trimmed);
        }
    }
    if !issue.description.trim().is_empty() {
        text.push_str("\n\n");
        text.push_str(issue.description.trim());
    }
    text
}

/// Dispatches one Architect decomposition request for a finding.
///
/// The Architect owns the "what": it decomposes a single finding into an
/// embedded step plan, and the typed `review/refineItem` response applies the
/// result. A batch therefore drains one finding per call, so every dispatch goes
/// through this single point.
async fn send_refine_request(
    server_writer: &mut RpcConnection,
    target: RefineTarget,
) -> anyhow::Result<()> {
    let params = ReviewRefineItemParams {
        review_id: Some(target.review_id),
        item_id: target.item_id,
        model: None,
        instructions: None,
    };
    send_request(
        server_writer,
        methods::REVIEW_REFINE_ITEM,
        serde_json::to_value(&params)?,
    )
    .await?;
    Ok(())
}

/// Opens the Architect decomposition dialog for the finding under the cursor.
///
/// The Architect owns the "what", so decomposition is an explicit request with
/// its own model choice and directives rather than an implicit retry: the dialog
/// carries the selected architect and the optional instructions into
/// `review/refineItem`. When more than one architect is configured, focus starts
/// on the model list for instant selection.
fn open_refine_dialog(st: &mut AppState) {
    if st.model.is_busy() || st.review.running || st.review.refining.is_some() {
        st.notify_warning(active_t("notify.model_busy"));
        return;
    }

    let (review_id, item_id, item_title) = match st.review.selected_row() {
        Some(VisibleReviewRow::ReviewItem {
            session_id,
            item_id,
            title,
            ..
        }) => (session_id, item_id, title),
        _ => {
            st.notify_info(active_t("notify.review_select_finding"));
            return;
        }
    };

    let review_title = st
        .review
        .sessions
        .iter()
        .find(|session| session.id == review_id)
        .map(|session| session.title.clone())
        .or_else(|| {
            st.review
                .session
                .as_ref()
                .filter(|session| session.id == review_id)
                .map(|session| session.title.clone())
        })
        .unwrap_or_else(|| review_id.clone());

    let entries = crate::views::plans::architect_entries(st);
    let mut dialog =
        ReviewRefineDialogState::new(review_id, review_title, item_id, item_title, entries);
    if dialog.selector.len() > 1 {
        dialog.focus = crate::views::plans::RefineDialogFocus::ModelList;
    }
    st.review.refine_dialog = Some(dialog);
}

/// Decomposes every finding that still awaits architectural refinement.
///
/// A finding in `REFINE` status has been classified by the Reviewer but not yet
/// broken into executable steps, so the batch is exactly the set of findings the
/// Architect still owes a decomposition for. The queue drains one finding per
/// `review/refineItem` response, and a failed or refused decomposition aborts the
/// remaining queue instead of spending further tokens.
async fn refine_all_findings(
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<()> {
    let target = {
        let mut st = state.lock().await;
        if st.model.is_busy() || st.review.running || st.review.refining.is_some() {
            st.notify_warning(active_t("notify.model_busy"));
            return Ok(());
        }
        let targets = st.review.pending_refine_targets();
        if targets.is_empty() {
            st.notify_info(active_t("notify.review_no_refine_needed"));
            return Ok(());
        }
        st.review.begin_refine_batch(targets)
    };
    let Some(target) = target else {
        return Ok(());
    };
    send_refine_request(server_writer, target).await?;
    state
        .lock()
        .await
        .notify_info(active_t("notify.review_refining"));
    Ok(())
}

pub async fn handle_review_key(
    key: KeyEvent,
    state: &Arc<Mutex<AppState>>,
    server_writer: &mut RpcConnection,
) -> anyhow::Result<InputResult> {
    let mut st = state.lock().await;

    // Resolve the key through the single source of truth so input dispatch and
    // the generated Help never drift (c-1). The command map already blocks bare
    // structural chords while a text editor is active, encodes the
    // case-differentiated chords (Shift+<key>) explicitly, and routes the
    // abstract primary modifier to the accordion commands (c-2).
    let action = match REVIEW_COMMAND_MAP.resolve(key, &st.tui_config.input, false) {
        Some(spec) => spec.action,
        None => return Ok(InputResult::Continue),
    };

    let rows_len = st.review.flatten_rows().len();
    let page = st.review.view_height.saturating_sub(2).max(1);

    match action {
        ReviewAction::Return => {
            st.view_mode = ViewMode::Repl;
        }
        ReviewAction::NavPrev => {
            if st.review.selected_index > 0 {
                st.review.selected_index -= 1;
                st.review.scroll_to_selected();
            }
        }
        ReviewAction::NavNext => {
            if rows_len > 0 && st.review.selected_index + 1 < rows_len {
                st.review.selected_index += 1;
                st.review.scroll_to_selected();
            }
        }
        ReviewAction::AccordionNext => {
            st.review.accordion_navigate(true);
        }
        ReviewAction::AccordionPrev => {
            st.review.accordion_navigate(false);
        }
        ReviewAction::SelectFirst => {
            st.review.selected_index = 0;
            st.review.scroll_to_selected();
        }
        ReviewAction::SelectLast => {
            if rows_len > 0 {
                st.review.selected_index = rows_len - 1;
                st.review.scroll_to_selected();
            }
        }
        // Paging moves the selection by one screenful and lets the cursor own
        // the viewport, so `PageUp` and `PageDown` keep the active row on screen
        // and never leave an invisible row as the target of the next command
        // (c-paging-cursor-sync).
        ReviewAction::PageUp => {
            if rows_len > 0 {
                st.review.selected_index =
                    st.review.selected_index.saturating_sub(usize::from(page));
                st.review.scroll_to_selected();
            }
        }
        ReviewAction::PageDown => {
            if rows_len > 0 {
                st.review.selected_index =
                    (st.review.selected_index + usize::from(page)).min(rows_len - 1);
                st.review.scroll_to_selected();
            }
        }
        ReviewAction::Recenter => {
            st.review.recenter_selected();
        }
        ReviewAction::FoldToggle => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader { session_id, .. } => {
                        st.review.toggle_session_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::SessionSummary { session_id, .. } => {
                        st.review.toggle_summary_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        ..
                    } => {
                        st.review.toggle_item_expanded(&session_id, item_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        item_id,
                        step_id,
                        ..
                    } => {
                        st.review
                            .toggle_subplan_item_expanded(&session_id, item_id, &step_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                }
            }
        }
        ReviewAction::Activate => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader { session_id, .. } => {
                        st.review.toggle_session_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::SessionSummary { session_id, .. } => {
                        st.review.toggle_summary_expanded(&session_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        title,
                        ..
                    } => {
                        execute_review_scope_action(
                            &mut st,
                            &session_id,
                            Some(item_id),
                            None,
                            &title,
                        );
                    }
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        item_id,
                        step_id,
                        ..
                    } => {
                        // Confirming on a step row targets that step, so the
                        // bounded turn implements exactly the inspected leaf
                        // (concept #c-4).
                        let title = finding_scope_title(&st, &session_id, item_id);
                        execute_review_scope_action(
                            &mut st,
                            &session_id,
                            Some(item_id),
                            Some(step_id),
                            &title,
                        );
                    }
                }
            }
        }
        ReviewAction::ExecuteAll => {
            execute_all_reviews_action(&mut st);
        }
        ReviewAction::Execute => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                match row {
                    VisibleReviewRow::SessionHeader {
                        session_id, title, ..
                    } => {
                        execute_review_scope_action(&mut st, &session_id, None, None, &title);
                    }
                    VisibleReviewRow::SessionSummary { session_id, .. } => {
                        execute_review_scope_action(&mut st, &session_id, None, None, &session_id);
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        title,
                        ..
                    } => {
                        execute_review_scope_action(
                            &mut st,
                            &session_id,
                            Some(item_id),
                            None,
                            &title,
                        );
                    }
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        item_id,
                        step_id,
                        ..
                    } => {
                        let title = finding_scope_title(&st, &session_id, item_id);
                        execute_review_scope_action(
                            &mut st,
                            &session_id,
                            Some(item_id),
                            Some(step_id),
                            &title,
                        );
                    }
                }
            }
        }
        ReviewAction::Discuss => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                let session_id = row.session_id().to_string();
                let session_opt = st
                    .review
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .cloned()
                    .or_else(|| {
                        st.review
                            .session
                            .as_ref()
                            .filter(|s| s.id == session_id)
                            .cloned()
                    });

                if let Some(session) = session_opt {
                    let mut focused = Vec::new();
                    if let VisibleReviewRow::ReviewItem { item_id, title, .. } = row {
                        focused.push((item_id, title));
                    }
                    st.prompt_modal = Some(crate::app::PromptModalState::new(
                        crate::app::PromptModalTarget::Review {
                            review_id: session.id.clone(),
                            review_title: session.title.clone(),
                            focused_items: focused,
                        },
                    ));
                }
            }
        }
        ReviewAction::SetFocus => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                let session_id = row.session_id().to_string();
                let review_title = st
                    .review
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .map(|s| s.title.clone())
                    .or_else(|| {
                        st.review
                            .session
                            .as_ref()
                            .filter(|s| s.id == session_id)
                            .map(|s| s.title.clone())
                    })
                    .unwrap_or_else(|| session_id.clone());
                let (focused, target_label) = match &row {
                    VisibleReviewRow::ReviewItem { item_id, title, .. } => (
                        vec![item_id.to_string()],
                        Some(format!("finding #{} '{}'", item_id, title)),
                    ),
                    _ => (Vec::new(), None),
                };
                let focus = tauqe_protocol::DiscussionFocus::review(session_id, focused);
                st.discussion_focus = focus.clone();
                let params = tauqe_protocol::DiscussionSetFocusParams { focus };
                let message = match target_label {
                    Some(label) => active_t("notify.review_focus_item_set")
                        .replacen("{}", &label, 1)
                        .replace("{}", &review_title),
                    None => active_t("notify.review_focus_set").replace("{}", &review_title),
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
        ReviewAction::ChangeStatus => {
            let sel_row = st.review.selected_row();
            if let Some(VisibleReviewRow::ReviewItem {
                session_id,
                item_id,
                title,
                status,
                ..
            }) = sel_row
            {
                // The status picker speaks the canonical `IssueStatus`
                // vocabulary shared by the plan and review domains.
                let cur_idx = match status {
                    IssueStatus::Discussion => 0,
                    IssueStatus::Todo => 1,
                    IssueStatus::InProgress => 2,
                    IssueStatus::Refine => 3,
                    IssueStatus::Fixed => 4,
                    IssueStatus::Skipped => 5,
                    IssueStatus::Rejected => 6,
                };
                st.status_dialog = Some(crate::app::StatusDialogState::new(
                    crate::app::StatusDialogTarget::ReviewItem {
                        review_id: Some(session_id),
                        item_id,
                        item_title: title,
                        current_status: status,
                    },
                    cur_idx,
                ));
            }
        }
        ReviewAction::ToggleHideClosed => {
            st.review.toggle_hide_closed();
            st.review.scroll_to_selected();
        }
        ReviewAction::FoldAll => {
            st.review.toggle_fold_all();
            st.review.clamp_selection();
            st.review.scroll_to_selected();
        }
        ReviewAction::DeleteReview => {
            let sel_row = st.review.selected_row();
            if let Some(row) = sel_row {
                st.confirm_delete_review = Some(row.session_id().to_string());
                st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            }
        }
        ReviewAction::Copy => {
            let sel_row = st.review.selected_row();
            let text = match sel_row {
                Some(VisibleReviewRow::ReviewItem {
                    session_id,
                    item_id,
                    ..
                }) => {
                    let s_opt = st
                        .review
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .or(st.review.session.as_ref());
                    s_opt
                        .and_then(|s| s.items.iter().find(|i| i.id == item_id))
                        .map(|item| format_item_for_clipboard(&ReviewIssue::from(item.clone())))
                }
                Some(VisibleReviewRow::SessionHeader { session_id, .. }) => {
                    let s_opt = st
                        .review
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .or(st.review.session.as_ref());
                    s_opt.map(|s| s.raw_markdown.clone())
                }
                Some(VisibleReviewRow::SessionSummary { description, .. }) => Some(description),
                Some(VisibleReviewRow::SubplanItem {
                    step_id,
                    title,
                    status,
                    files,
                    readonly_files,
                    details,
                    ..
                }) => {
                    let mut text = format!("#{} [{}] {}", step_id, status, title);
                    if !files.is_empty() {
                        text.push_str("\nfiles: ");
                        text.push_str(&files.join(", "));
                    }
                    if !readonly_files.is_empty() {
                        text.push_str("\nref: ");
                        text.push_str(&readonly_files.join(", "));
                    }
                    if let Some(details) = details {
                        let trimmed = details.trim();
                        if !trimmed.is_empty() {
                            text.push_str("\n\n");
                            text.push_str(trimmed);
                        }
                    }
                    Some(text)
                }
                None => None,
            };

            if let Some(text) = text {
                drop(st);
                let res = crate::clipboard::copy_to_clipboard(&text);
                let mut st = state.lock().await;
                match res {
                    crate::clipboard::CopyResult::Native => {
                        st.notify_success(active_t("notify.review_copied"));
                    }
                    crate::clipboard::CopyResult::Osc52Only => {
                        st.notify_info(active_t("notify.review_copied_osc52"));
                    }
                    crate::clipboard::CopyResult::Failed => {
                        st.notify_error(active_t("notify.review_copy_failed"));
                    }
                }
            }
        }
        ReviewAction::RunReview => {
            if st.model.is_busy() {
                st.review.error = Some(active_t("notify.review_cannot_start_busy"));
            } else if st.context.items.is_empty() {
                st.review.error = Some(active_t("notify.review_context_empty"));
            } else {
                let reviewer_entries = st
                    .model_choice
                    .roles
                    .as_ref()
                    .and_then(|roles| roles.entries.as_ref())
                    .map(|entries| entries.reviewer.clone())
                    .filter(|entries| !entries.is_empty())
                    .unwrap_or_else(|| {
                        let fallback_model = st
                            .model_choice
                            .resolve(&ModelSelection::Role(ModelRole::Reviewer))
                            .unwrap_or_else(|| st.active_model.clone());
                        let fallback_effort = st
                            .model_choice
                            .roles
                            .as_ref()
                            .and_then(|roles| roles.efforts.as_ref())
                            .and_then(|efforts| efforts.reviewer);
                        vec![tauqe_protocol::ModelEntry::new(
                            fallback_model,
                            fallback_effort,
                        )]
                    });
                let files_count = st.context.items.len();
                let estimated_tokens = st.context.total_estimated_tokens as usize;
                st.review.error = None;
                let mut dialog =
                    ReviewDialogState::new(files_count, estimated_tokens, reviewer_entries);
                // Open on the reviewer list when more than one model is
                // configured so it can be picked instantly with the digit
                // shortcuts; a single model leaves focus on the editor
                // (c-magit-key-navigation).
                if dialog.selector.len() > 1 {
                    dialog.focus = crate::app::ReviewDialogFocus::ModelList;
                }
                st.review_dialog = Some(dialog);
            }
        }
        // Decompose the selected finding into an embedded step plan via the
        // Architect role: the dialog carries the architect model and directives.
        ReviewAction::Refine => {
            open_refine_dialog(&mut st);
        }
        // Decompose every finding that still awaits architectural refinement.
        ReviewAction::RefineAll => {
            drop(st);
            refine_all_findings(state, server_writer).await?;
        }
        // Refresh review sessions from the server.
        ReviewAction::Refresh => {
            drop(st);
            send_request(server_writer, methods::REVIEW_LIST, serde_json::json!({})).await?;
        }
    }

    Ok(InputResult::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_map_chords_are_unique() {
        let chords: Vec<KeyChord> = REVIEW_COMMAND_MAP
            .specs()
            .iter()
            .map(|spec| spec.chord)
            .collect();
        assert!(!chords.is_empty());
        for (index, chord) in chords.iter().enumerate() {
            for other in chords.iter().skip(index + 1) {
                assert_ne!(chord, other, "duplicate key chord in REVIEW_COMMAND_MAP");
            }
        }
    }

    #[tokio::test]
    async fn test_review_dialog_uses_configured_reviewer_role() {
        use tauqe_protocol::{ModelRef, ModelRolesEffort, ModelRolesSummary, ReasoningEffort};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.context.items.push(tauqe_protocol::ContextItem {
            path: "crates/core/src/lib.rs".to_string(),
            access: tauqe_protocol::ContextAccess::ReadOnly,
            size_bytes: 100,
            estimated_tokens: 25,
            layer: tauqe_protocol::ContextLayer::User,
            insertion_seq: 0,
        });

        let rev_model = ModelRef::openrouter("anthropic/claude-3.5-sonnet");
        let rev_entry =
            tauqe_protocol::ModelEntry::new(rev_model.clone(), Some(ReasoningEffort::High));
        st.model_choice.roles = Some(ModelRolesSummary {
            architect: ModelRef::openrouter("test-arch"),
            coder: ModelRef::openrouter("test-coder"),
            primary: ModelRef::openrouter("test-pri"),
            reviewer: rev_model.clone(),
            scribe: ModelRef::openrouter("test-scribe"),
            efforts: Some(ModelRolesEffort {
                architect: None,
                coder: None,
                primary: None,
                reviewer: Some(ReasoningEffort::High),
                scribe: None,
            }),
            entries: Some(tauqe_protocol::ModelRolesEntries {
                reviewer: vec![rev_entry],
                ..Default::default()
            }),
        });

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // `N` creates a new review session over the current context.
        let key = KeyEvent::new(KeyCode::Char('N'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        let dialog = st.review_dialog.as_ref().expect("dialog should be opened");
        assert_eq!(dialog.selector.role(), ModelRole::Reviewer);
        let selected = dialog.selected_entry().expect("entry must exist");
        assert_eq!(selected.model, rev_model);
        assert_eq!(selected.effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn test_execute_review_scope_action_defaults_to_safe_cancel() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut st = crate::app::test_state();
        st.model.status = crate::views::repl::TurnStatus::Idle;
        // Start on the destructive button to prove the action resets the focus.
        st.confirm_button = crate::app::ConfirmDialogButton::Confirm;

        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session rev-1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewIssue {
                id: 1,
                title: "Finding".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session.clone()];
        st.review.session = Some(session);

        execute_review_scope_action(&mut st, "rev-1", Some(1), None, "Finding");

        assert_eq!(
            st.confirm_button,
            crate::app::ConfirmDialogButton::Cancel,
            "the confirmation must default to safe cancellation"
        );
        let target = st
            .confirm_execute_review
            .as_ref()
            .expect("confirmation state should be armed");
        assert_eq!(target.review_id, Some("rev-1".to_string()));
        assert_eq!(target.item_id, Some(1));
        assert_eq!(target.items.len(), 1);
    }

    #[test]
    fn test_execute_all_reviews_action_discussion_only_warns() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut st = crate::app::test_state();
        let session = ReviewSession {
            id: "rev-disc".to_string(),
            title: "Discussion Only Session".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewIssue {
                    id: 1,
                    title: "Finding 1".to_string(),
                    model: None,
                    issue_type: IssueType::Architecture,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 2".to_string(),
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        execute_all_reviews_action(&mut st);

        assert!(
            st.confirm_execute_review.is_none(),
            "dialog must not open when all findings are in DISCUSSION"
        );
        let notif = st
            .active_notification()
            .expect("notification should be surfaced");
        assert!(
            notif.text.contains("DISCUSSION"),
            "expected notification warning about DISCUSSION status, got: {}",
            notif.text
        );
    }

    #[test]
    fn test_execute_all_reviews_action_with_todo_opens_modal() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut st = crate::app::test_state();
        let session1 = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session 1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewIssue {
                    id: 1,
                    title: "Finding 1".to_string(),
                    model: None,
                    issue_type: IssueType::Architecture,
                    status: IssueStatus::Todo,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 2".to_string(),
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Fixed,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
        };
        let session2 = ReviewSession {
            id: "rev-2".to_string(),
            title: "Session 2".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![
                ReviewIssue {
                    id: 1,
                    title: "Finding 3".to_string(),
                    model: None,
                    issue_type: IssueType::Bug,
                    status: IssueStatus::InProgress,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 4".to_string(),
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session1, session2];

        execute_all_reviews_action(&mut st);

        let target = st
            .confirm_execute_review
            .as_ref()
            .expect("modal confirmation must be armed");
        assert_eq!(
            target.review_id, None,
            "cross-session execution must have review_id: None"
        );
        assert_eq!(target.review_title, "All Review Sessions");
        assert_eq!(st.confirm_button, crate::app::ConfirmDialogButton::Cancel);
        assert_eq!(
            target.items.len(),
            2,
            "must include exactly the Todo and InProgress items"
        );
        assert_eq!(target.items[0].title, "Finding 1");
        assert_eq!(target.items[1].title, "Finding 3");
    }

    #[test]
    fn test_execute_all_reviews_action_all_fixed_notifies() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut st = crate::app::test_state();
        let session = ReviewSession {
            id: "rev-fixed".to_string(),
            title: "All Fixed Session".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewIssue {
                id: 1,
                title: "Finding 1".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Fixed,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        execute_all_reviews_action(&mut st);

        assert!(st.confirm_execute_review.is_none());
        let notif = st
            .active_notification()
            .expect("notification should be set");
        assert!(notif.text.contains("FIXED"));
    }

    #[test]
    fn test_execute_all_reviews_action_includes_refine_findings() {
        let mut st = crate::app::test_state();
        // A finding awaiting decomposition is executable just like a TODO one:
        // the server refines it before coding (#c-1).
        st.review.sessions = vec![
            session_with_status("rev-refine", 1, IssueStatus::Refine),
            todo_session("rev-todo", 2),
        ];

        execute_all_reviews_action(&mut st);

        let target = st
            .confirm_execute_review
            .as_ref()
            .expect("modal confirmation must be armed");
        assert_eq!(
            target.items.len(),
            2,
            "must include both the Refine and the Todo finding"
        );
        assert_eq!(target.items[0].title, "Todo Finding");
        assert_eq!(target.items[1].title, "Todo Finding");
        assert_eq!(target.review_id, None);
        assert_eq!(st.confirm_button, crate::app::ConfirmDialogButton::Cancel);
    }

    #[tokio::test]
    async fn test_ctrl_l_recenters_review() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = (1..=15)
            .map(|i| todo_session(&format!("rev-{i}"), i))
            .collect();
        st.review.selected_index = 8;
        st.review.view_height = 5;
        st.review.content_width = 80;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        let key = KeyEvent::new(KeyCode::Char('l'), crossterm::event::KeyModifiers::CONTROL);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let scroll_first = state.lock().await.review.scroll;
        handle_review_key(key, &state, &mut conn).await.unwrap();
        let scroll_second = state.lock().await.review.scroll;

        assert_ne!(
            scroll_first, scroll_second,
            "C-l must cycle recenter positions in review view"
        );
    }

    #[tokio::test]
    async fn test_handle_review_key_capital_x_triggers_batch_execute() {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        let session = ReviewSession {
            id: "rev-1".to_string(),
            title: "Session 1".to_string(),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewIssue {
                id: 1,
                title: "Todo Finding".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
        };
        st.review.sessions = vec![session];

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('X'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        let target = st.confirm_execute_review.as_ref().expect("modal must open");
        assert_eq!(target.review_id, None);
        assert_eq!(target.items.len(), 1);
    }

    fn todo_session(id: &str, item_id: u32) -> tauqe_protocol::ReviewSession {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        ReviewSession {
            id: id.to_string(),
            title: format!("Session {id}"),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: vec![ReviewIssue {
                id: item_id,
                title: "Todo Finding".to_string(),
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                plan: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
        }
    }

    fn session_with_status(
        id: &str,
        item_id: u32,
        status: IssueStatus,
    ) -> tauqe_protocol::ReviewSession {
        let mut session = todo_session(id, item_id);
        if let Some(item) = session.items.first_mut() {
            item.status = status;
        }
        session
    }

    #[tokio::test]
    async fn test_lowercase_r_opens_the_decomposition_dialog() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        // Row 0 is the session header; row 1 is the finding.
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('r'), crossterm::event::KeyModifiers::NONE);
        let res = handle_review_key(key, &state, &mut conn).await.unwrap();
        assert_eq!(res, InputResult::Continue);

        let st = state.lock().await;
        // The dialog opens instead of dispatching immediately, so the developer
        // can choose the architect and add directives before spending a turn.
        let dialog = st
            .review
            .refine_dialog
            .as_ref()
            .expect("the decomposition dialog must open");
        assert_eq!(dialog.review_id, "rev-1");
        assert_eq!(dialog.item_id, 1);
        assert_eq!(dialog.selector.len(), 1);
        assert!(st.review.refining.is_none());
    }

    #[tokio::test]
    async fn test_lowercase_r_on_session_header_requires_a_finding() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        st.review.selected_index = 0; // session header row

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('r'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.review.refining.is_none());
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Select a finding to decompose into steps (r)")
        );
    }

    #[tokio::test]
    async fn test_capital_r_decomposes_every_finding_awaiting_refinement() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![
            session_with_status("rev-1", 1, IssueStatus::Refine),
            todo_session("rev-2", 2),
        ];
        st.review.selected_index = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        // Only the REFINE finding is owed a decomposition, so the batch
        // dispatches it and leaves nothing queued behind.
        assert_eq!(st.review.refining.as_deref(), Some("rev-1"));
        assert!(st.review.refine_queue.is_empty());
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Decomposing finding with the Architect...")
        );
    }

    #[tokio::test]
    async fn test_capital_r_without_refinable_findings_notifies() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('R'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(st.review.refining.is_none());
        assert!(st.review.refine_queue.is_empty());
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("No findings require architectural decomposition")
        );
    }

    /// A session holding `count` collapsed findings, used by the paging tests.
    fn session_with_items(id: &str, count: u32) -> tauqe_protocol::ReviewSession {
        use tauqe_protocol::{IssueStatus, IssueType, ReviewIssue, ReviewSession};

        ReviewSession {
            id: id.to_string(),
            title: format!("Session {id}"),
            created_at: 0,
            model: tauqe_protocol::ModelRef::openrouter("test-model"),
            description: None,
            user_prompt: None,
            target_files: Vec::new(),
            items: (1..=count)
                .map(|n| {
                    ReviewIssue {
                        id: n,
                        title: format!("Finding {n}"),
                        model: None,
                        issue_type: IssueType::Architecture,
                        status: IssueStatus::Todo,
                        description: String::new(),
                        recommendations: Vec::new(),
                        files: Vec::new(),
                        readonly_files: Vec::new(),
                        plan: Vec::new(),
                        note: None,
                    }
                    .into()
                })
                .collect(),
            raw_markdown: String::new(),
        }
    }

    /// Asserts that the row under the cursor lies entirely inside the viewport.
    fn assert_cursor_visible(view: &mut crate::views::review::ReviewViewState) {
        let index = view.selected_index;
        let width = view.content_width.max(10);
        let rows = view.flatten_rows();
        let (lines, offsets) = crate::views::review::compute_unified_review_lines(
            &rows,
            index,
            width,
            view.theme_mode,
            &mut view.markdown_cache,
        );
        let start = offsets.get(index).copied().unwrap_or(0);
        let end = offsets.get(index + 1).copied().unwrap_or(lines.len());
        let scroll = view.scroll as usize;
        let height = view.view_height as usize;
        assert!(
            scroll <= start && scroll + height >= end,
            "the selected row {start}..{end} must stay visible in {scroll}..{}",
            scroll + height
        );
    }

    /// `PageDown` and `PageUp` move the active row by one screenful and the
    /// viewport follows it, so the cursor never leaves the visible area and an
    /// invisible row never becomes the target of the next command
    /// (c-paging-cursor-sync).
    #[tokio::test]
    async fn test_paging_moves_the_selection_and_keeps_it_visible() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![session_with_items("rev-paging", 60)];
        st.review.view_height = 10;
        st.review.content_width = 80;
        st.review.selected_index = 1;
        st.review.scroll = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let down = KeyEvent::new(KeyCode::PageDown, crossterm::event::KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::PageUp, crossterm::event::KeyModifiers::NONE);

        // One page is the viewport height minus the two reserved rows.
        handle_review_key(down, &state, &mut conn).await.unwrap();
        {
            let mut guard = state.lock().await;
            assert_eq!(guard.review.selected_index, 9);
            assert_cursor_visible(&mut guard.review);
        }

        // Paging to the end stops on the last row instead of running past it.
        for _ in 0..100 {
            handle_review_key(down, &state, &mut conn).await.unwrap();
        }
        {
            let mut guard = state.lock().await;
            assert_eq!(guard.review.selected_index, 60);
            assert_cursor_visible(&mut guard.review);
        }

        // Paging back up returns one screenful with the cursor still on screen.
        handle_review_key(up, &state, &mut conn).await.unwrap();
        {
            let mut guard = state.lock().await;
            assert_eq!(guard.review.selected_index, 52);
            assert_cursor_visible(&mut guard.review);
        }
    }

    /// `Home` and `End` move the cursor to the extremes of the tree and keep it
    /// inside the viewport (c-scroll-visibility).
    #[tokio::test]
    async fn test_home_and_end_keep_the_selection_visible() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![session_with_items("rev-home-end", 60)];
        st.review.view_height = 10;
        st.review.content_width = 80;
        st.review.selected_index = 0;
        st.review.scroll = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let end = KeyEvent::new(KeyCode::End, crossterm::event::KeyModifiers::NONE);
        handle_review_key(end, &state, &mut conn).await.unwrap();
        {
            let mut guard = state.lock().await;
            let last = guard.review.row_count() - 1;
            assert_eq!(guard.review.selected_index, last);
            assert!(
                guard.review.scroll > 0,
                "the last row must scroll the viewport down"
            );
            assert_cursor_visible(&mut guard.review);
        }

        let home = KeyEvent::new(KeyCode::Home, crossterm::event::KeyModifiers::NONE);
        handle_review_key(home, &state, &mut conn).await.unwrap();
        {
            let mut guard = state.lock().await;
            assert_eq!(guard.review.selected_index, 0);
            assert_eq!(
                guard.review.scroll, 0,
                "the first row must reveal the document top"
            );
            assert_cursor_visible(&mut guard.review);
        }
    }
}
