use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent};
use tauqe_protocol::review::DEFAULT_REVIEW_GROUP_ID;
use tauqe_protocol::{
    methods, IssueStatus, ModelRole, ModelSelection, PlanItem, ReviewGroup, ReviewGroupKind,
    ReviewIssue, ReviewSession,
};
use tokio::sync::Mutex;

use crate::app::{AppState, ConfirmReviewAnalyzeState, ReviewDialogState, ViewMode};
use crate::command::CommandCategory::{Essential, Helpers, Interface};
use crate::command::{CommandMap, CommandSpec, KeyChord};
use crate::i18n::engine::active_t;
use crate::input::InputResult;
use crate::rpc::{send_request, RpcConnection};

use super::VisibleReviewRow;

/// Keyboard actions available in the Review view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    Return,
    Activate,
    Execute,
    ExecuteAll,
    RunReview,
    Discuss,
    Analyze,
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
    // Starting a new review session is a creation verb, so it takes `N` and
    // refresh keeps the canonical `g` (Conventions §4.5, §4.6).
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
    // Case-differentiated analysis chords, declared before the bare `a` below:
    // Shift+A launches the Architect post-analysis over the selected session
    // (#c-3), so the global fold keeps only the bare `a` and the uppercase
    // letter no longer aliases it.
    CommandSpec::new(
        KeyChord::Bare(KeyCode::Char('A')),
        ReviewAction::Analyze,
        Essential,
        "cmd.review.analyze",
    ),
    CommandSpec::new(
        KeyChord::Shift(KeyCode::Char('a')),
        ReviewAction::Analyze,
        Essential,
        "cmd.review.analyze",
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

/// Whether a finding can still produce execution work.
///
/// A finding is actionable while it is `TODO` or `IN_PROGRESS`: a finding of a
/// simple group runs as one isolated turn, while a finding of an architectural
/// group is resolved only through its group plan and is therefore never queued
/// on its own (c-1, c-2).
fn is_actionable(status: IssueStatus) -> bool {
    matches!(status, IssueStatus::Todo | IssueStatus::InProgress)
}

/// Number of leaf steps a plan carries.
///
/// A container step delegates its work to its children, so the executable work
/// of a plan is counted at its leaves, exactly as the confirmation card counts
/// it (c-2, c-5).
fn plan_leaf_count(steps: &[PlanItem]) -> usize {
    steps
        .iter()
        .map(|step| {
            if step.is_leaf() {
                1
            } else {
                plan_leaf_count(&step.children)
            }
        })
        .sum()
}

/// Display title of a review group, falling back to its identifier.
fn group_display_title(group: &ReviewGroup) -> String {
    let title = group.title.trim();
    if title.is_empty() {
        group.id.clone()
    } else {
        title.to_string()
    }
}
/// Execution target of one review group: its class, its display title, and the
/// unified root-cause plan it owns.
///
/// The group class and title travel with the header row, but the unified plan
/// itself lives in the session, so the group header and its plan branch resolve
/// their execution target here. An undeclared group id renders through its
/// protective orphan bucket, so it resolves to a simple group that owns no
/// plan.
fn resolve_group_target(
    state: &AppState,
    session_id: &str,
    group_id: &str,
) -> (ReviewGroupKind, String, Vec<PlanItem>) {
    match find_session(state, session_id) {
        Some(session) => session
            .effective_groups()
            .into_iter()
            .find(|group| group.id == group_id)
            .map(|group| (group.kind, group_display_title(&group), group.subplan))
            .unwrap_or_else(|| {
                (
                    ReviewGroupKind::Simple,
                    super::projection::orphan_title(group_id),
                    Vec::new(),
                )
            }),
        None => (ReviewGroupKind::Simple, group_id.to_string(), Vec::new()),
    }
}
/// Resolves a stored review session from the view mirror.
fn find_session(state: &AppState, session_id: &str) -> Option<ReviewSession> {
    state
        .review
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .cloned()
        .or_else(|| {
            state
                .review
                .session
                .as_ref()
                .filter(|session| session.id == session_id)
                .cloned()
        })
}

/// Number of architectural root-cause plans an actionable queue carries.
///
/// Every architectural group owning at least one actionable finding
/// contributes exactly one coherent plan to the queue the server executes —
/// stored already or projected just-in-time — so the confirmation states how
/// many root causes the run resolves through its groups (c-2, c-4).
fn architectural_plan_count(session: &ReviewSession, items: &[ReviewIssue]) -> usize {
    let groups = session.effective_groups();
    let mut counted: Vec<&str> = Vec::new();
    for item in items {
        let group_id = item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
        let architectural = groups
            .iter()
            .any(|group| group.id == group_id && group.kind == ReviewGroupKind::Architectural);
        if architectural && !counted.contains(&group_id) {
            counted.push(group_id);
        }
    }
    counted.len()
}

/// Localized description of a mixed review queue.
///
/// The queue carries the per-finding work of simple groups plus one root-cause
/// plan per architectural group, so the plan count is stated beside the scope
/// and the confirmation card appends the finding count itself (c-2, c-4).
fn queue_scope_title(group_plans: usize) -> String {
    let mut scope = active_t("review.session_queue_scope");
    if group_plans > 0 {
        scope.push_str(
            &active_t("dialog.execute_review.group_plans_count").replacen(
                "{}",
                &group_plans.to_string(),
                1,
            ),
        );
    }
    scope
}

/// Arms the point-execution confirmation for one finding.
///
/// A finding of an architectural group is a symptom of one shared root cause,
/// so it is never executed on its own: the handler refuses it and points at the
/// group plan instead (c-1). Any other finding opens the point form with its
/// own Coder choice and directives (Conventions §6).
fn execute_review_finding_action(st: &mut AppState, session_id: &str, item_id: u32) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let session = match find_session(st, session_id) {
        Some(session) => session,
        None => {
            st.notify_error(active_t("notify.review_not_found").replace("{}", session_id));
            return;
        }
    };

    let item = match session.find_item(item_id) {
        Some(item) => item.clone(),
        None => {
            st.notify_error(
                active_t("notify.review_item_not_found").replace("{}", &item_id.to_string()),
            );
            return;
        }
    };

    // A finding of an architectural group is a symptom: executing it alone
    // would fix one manifestation and leave the shared root cause in place
    // (c-1).
    let groups = session.effective_groups();
    let group_id = item.group.as_deref().unwrap_or(DEFAULT_REVIEW_GROUP_ID);
    if let Some(group) = groups
        .iter()
        .find(|group| group.id == group_id && group.kind == ReviewGroupKind::Architectural)
    {
        st.notify_warning(
            active_t("notify.review_item_in_architectural_group")
                .replacen("{}", &item.id.to_string(), 1)
                .replacen("{}", &group_display_title(group), 1),
        );
        return;
    }

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
        // Point execution opens the Coder form so the implementing model and
        // the directives are chosen before a bounded turn runs (c-2).
        IssueStatus::Todo | IssueStatus::InProgress => {
            let session_title = if session.title.trim().is_empty() {
                session.id.clone()
            } else {
                session.title.clone()
            };
            let coder_entries = st.coder_entries();
            st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
            st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
                Some(session.id.clone()),
                session_title,
                item.title.clone(),
                Some(item.id),
                None,
                vec![ReviewIssue::from(item)],
                coder_entries,
            ));
        }
    }
}

/// Arms the execution confirmation for the whole actionable queue of a session.
///
/// The queue mixes the per-finding work of simple groups with one root-cause
/// plan per architectural group, so the server executes an architectural group
/// as a coherent sequence and projects a missing plan just-in-time (c-2, c-4).
fn execute_review_session_action(st: &mut AppState, session_id: &str) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let session = match find_session(st, session_id) {
        Some(session) => session,
        None => {
            st.notify_error(active_t("notify.review_not_found").replace("{}", session_id));
            return;
        }
    };

    let todo_items: Vec<ReviewIssue> = session
        .items
        .iter()
        .filter(|item| is_actionable(item.status))
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

    let scope_title = queue_scope_title(architectural_plan_count(&session, &todo_items));
    let session_title = if session.title.trim().is_empty() {
        session.id.clone()
    } else {
        session.title.clone()
    };
    let coder_entries = st.coder_entries();
    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
    // The session root launches the implicit whole-session queue: the flag
    // records that the confirmation names the session itself and leaves the
    // member list — architectural root-cause plans and independent findings
    // alike — to the server, exactly as the cross-session batch does (c-1, c-2,
    // c-3).
    st.confirm_execute_review = Some(
        crate::app::ConfirmExecuteReviewState::new(
            Some(session.id.clone()),
            session_title,
            scope_title,
            None,
            None,
            todo_items,
            coder_entries,
        )
        .with_whole_session(),
    );
}

/// Arms the execution confirmation for one rendered group.
///
/// The class of the group decides the shape. An architectural group owns one
/// root-cause plan, so it always arms the binary group-plan confirmation and
/// runs on the configured roles while the Architect projects a missing plan
/// just-in-time (c-2, c-4). A simple group keeps the per-finding queue it
/// always had (c-1).
fn execute_review_group_action(
    st: &mut AppState,
    session_id: &str,
    group_id: &str,
    kind: ReviewGroupKind,
    group_title: &str,
    subplan: &[PlanItem],
) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let session = match find_session(st, session_id) {
        Some(session) => session,
        None => {
            st.notify_error(active_t("notify.review_not_found").replace("{}", session_id));
            return;
        }
    };

    let items: Vec<ReviewIssue> = st
        .review
        .group_findings(&session, group_id)
        .into_iter()
        .filter(|item| is_actionable(item.status))
        .map(ReviewIssue::from)
        .collect();

    if items.is_empty() {
        st.notify_info(active_t("notify.review_no_todo_section"));
        return;
    }

    let session_title = if session.title.trim().is_empty() {
        session.id.clone()
    } else {
        session.title.clone()
    };
    let coder_entries = st.coder_entries();
    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;

    let target = match kind {
        ReviewGroupKind::Architectural => {
            let scope_title = if subplan.is_empty() {
                active_t("review.group_plan_scope_jit").replacen("{}", group_title, 1)
            } else {
                active_t("review.group_plan_scope")
                    .replacen("{}", group_title, 1)
                    .replacen("{}", &plan_leaf_count(subplan).to_string(), 1)
            };
            crate::app::ConfirmExecuteReviewState::new(
                Some(session.id.clone()),
                session_title,
                scope_title,
                None,
                None,
                items,
                coder_entries,
            )
            .with_group(group_id.to_string(), group_title.to_string())
        }
        ReviewGroupKind::Simple => crate::app::ConfirmExecuteReviewState::new(
            Some(session.id.clone()),
            session_title,
            group_title.to_string(),
            None,
            None,
            items,
            coder_entries,
        ),
    };
    st.confirm_execute_review = Some(target);
}

/// Arms the batch confirmation for every actionable finding across all sessions.
///
/// The scope states both halves of the mixed queue the server builds: the
/// per-finding work of simple groups and the architectural root-cause plans it
/// executes — or projects just-in-time — for the groups (c-2, c-4).
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
    let mut group_plans = 0;
    let mut total_items = 0;
    let mut discussion_count = 0;
    let mut fixed_count = 0;

    for session in &sessions {
        let mut session_items: Vec<ReviewIssue> = Vec::new();
        for item in &session.items {
            total_items += 1;
            match item.status {
                IssueStatus::Todo | IssueStatus::InProgress => {
                    session_items.push(ReviewIssue::from(item.clone()));
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
        group_plans += architectural_plan_count(session, &session_items);
        todo_items.extend(session_items);
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
        active_t("review.all_sessions_title"),
        queue_scope_title(group_plans),
        None,
        None,
        todo_items,
        coder_entries,
    ));
}

/// Arms the point-execution confirmation for one step of a group plan.
///
/// A subplan step belongs to the unified plan of its group, so the confirmation
/// carries the group target and the concrete step while still addressing one
/// targeted unit of work: the Coder selector and the directives editor stay
/// available (c-5, Conventions §6).
fn execute_review_group_step_action(
    st: &mut AppState,
    session_id: &str,
    group_id: &str,
    step_id: &str,
    step_title: &str,
) {
    if st.model.is_busy() {
        st.notify_warning(active_t("notify.model_busy_esc"));
        return;
    }

    let session = match find_session(st, session_id) {
        Some(session) => session,
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
    let group_title = session
        .effective_groups()
        .into_iter()
        .find(|group| group.id == group_id)
        .map(|group| group_display_title(&group))
        .unwrap_or_else(|| group_id.to_string());

    let items: Vec<ReviewIssue> = st
        .review
        .group_findings(&session, group_id)
        .into_iter()
        .filter(|item| is_actionable(item.status))
        .map(ReviewIssue::from)
        .collect();

    let scope_title = active_t("review.group_step_scope")
        .replacen("{}", step_title, 1)
        .replacen("{}", &group_title, 1);

    let coder_entries = st.coder_entries();
    st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
    st.confirm_execute_review = Some(
        crate::app::ConfirmExecuteReviewState::new(
            Some(session.id.clone()),
            session_title,
            scope_title,
            None,
            Some(step_id.to_string()),
            items,
            coder_entries,
        )
        .with_group(group_id.to_string(), group_title),
    );
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
                        group_id,
                        step_id,
                        ..
                    } => {
                        st.review
                            .toggle_subplan_item_expanded(&session_id, &group_id, &step_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    VisibleReviewRow::GroupHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        st.review.toggle_group_expanded(&session_id, &group_id);
                        st.review.clamp_selection();
                        st.review.scroll_to_selected();
                    }
                    // The plan branch folds independently of the group header
                    // above it and of the findings below it, so folding it never
                    // hides a finding (concept #c-1).
                    VisibleReviewRow::GroupPlanHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        st.review
                            .toggle_plan_branch_expanded(&session_id, &group_id);
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
                        ..
                    } => {
                        execute_review_finding_action(&mut st, &session_id, item_id);
                    }
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        group_id,
                        step_id,
                        title,
                        ..
                    } => {
                        // Confirming on a step row executes exactly that step of
                        // the group plan with the configured Coder (c-5).
                        execute_review_group_step_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            &step_id,
                            &title,
                        );
                    }
                    // Confirming on a group header resolves through the group
                    // class: an architectural group arms its root-cause plan,
                    // while a simple group queues its findings (c-4, c-5). The
                    // unified plan lives in the session, not in the row.
                    VisibleReviewRow::GroupHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        let (kind, title, subplan) =
                            resolve_group_target(&st, &session_id, &group_id);
                        execute_review_group_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            kind,
                            &title,
                            &subplan,
                        );
                    }
                    // The plan branch names the same root-cause plan as the
                    // group header above it, so both rows arm one target
                    // (concept #c-1).
                    VisibleReviewRow::GroupPlanHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        let (kind, title, subplan) =
                            resolve_group_target(&st, &session_id, &group_id);
                        execute_review_group_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            kind,
                            &title,
                            &subplan,
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
                    // A session header or summary queues the whole session, so
                    // the run covers every simple finding and every group plan
                    // the server builds from it (c-2, c-4).
                    VisibleReviewRow::SessionHeader { session_id, .. }
                    | VisibleReviewRow::SessionSummary { session_id, .. } => {
                        execute_review_session_action(&mut st, &session_id);
                    }
                    VisibleReviewRow::ReviewItem {
                        session_id,
                        item_id,
                        ..
                    } => {
                        execute_review_finding_action(&mut st, &session_id, item_id);
                    }
                    VisibleReviewRow::SubplanItem {
                        session_id,
                        group_id,
                        step_id,
                        title,
                        ..
                    } => {
                        execute_review_group_step_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            &step_id,
                            &title,
                        );
                    }
                    // A group executes exactly its own scope, never the whole
                    // session: an architectural group arms its root-cause plan,
                    // a simple group queues its own findings (c-1, c-4, c-5).
                    VisibleReviewRow::GroupHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        let (kind, title, subplan) =
                            resolve_group_target(&st, &session_id, &group_id);
                        execute_review_group_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            kind,
                            &title,
                            &subplan,
                        );
                    }
                    // The branch header of a group plan is the group plan, so
                    // the execution chord arms the identical target (c-1).
                    VisibleReviewRow::GroupPlanHeader {
                        session_id,
                        group_id,
                        ..
                    } => {
                        let (kind, title, subplan) =
                            resolve_group_target(&st, &session_id, &group_id);
                        execute_review_group_action(
                            &mut st,
                            &session_id,
                            &group_id,
                            kind,
                            &title,
                            &subplan,
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
                    let focused: Vec<(u32, String)> = match &row {
                        VisibleReviewRow::ReviewItem { item_id, title, .. } => {
                            vec![(*item_id, title.clone())]
                        }
                        VisibleReviewRow::GroupHeader { group_id, .. } => st
                            .review
                            .group_findings(&session, group_id)
                            .into_iter()
                            .map(|item| (item.id, item.title))
                            .collect(),
                        _ => Vec::new(),
                    };
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
                    IssueStatus::Fixed => 3,
                    IssueStatus::Skipped => 4,
                    IssueStatus::Rejected => 5,
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
                Some(VisibleReviewRow::GroupHeader {
                    title,
                    findings_count,
                    ..
                }) => Some(format!(
                    "{} ({})",
                    title,
                    active_t("review.group_findings_count").replacen(
                        "{}",
                        &findings_count.to_string(),
                        1
                    )
                )),
                Some(VisibleReviewRow::GroupPlanHeader {
                    title, done, total, ..
                }) => Some(format!(
                    "{} [{}]",
                    title,
                    active_t("review.steps_count")
                        .replacen("{}", &done.to_string(), 1)
                        .replacen("{}", &total.to_string(), 1)
                )),
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
                // The handshake file list feeds the dialog's directory section
                // and its live match counter, so the launch is validated locally
                // with the same matcher the server applies (c-2, c-3).
                let mut dialog = ReviewDialogState::new(
                    files_count,
                    estimated_tokens,
                    reviewer_entries,
                    st.all_repo_files.clone(),
                )
                .with_presets(st.review_presets.clone());
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
        // Launch the Architect post-analysis over the session the cursor rests
        // on. The classification is a costly LLM layer over immutable findings,
        // so `A` arms a confirmation whose safe default cancels it instead of
        // spending tokens on a stray keystroke (c-4).
        ReviewAction::Analyze => {
            if st.model.is_busy() || st.review.running || st.review.analyzing {
                st.notify_warning(active_t("notify.model_busy"));
            } else {
                match st.review.selected_row() {
                    Some(row) => {
                        let review_id = row.session_id().to_string();
                        let review_title = st
                            .review
                            .sessions
                            .iter()
                            .chain(st.review.session.iter())
                            .find(|session| session.id == review_id)
                            .map(|session| session.title.trim())
                            .filter(|title| !title.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| review_id.clone());
                        st.confirm_button = crate::app::ConfirmDialogButton::Cancel;
                        st.confirm_review_analyze = Some(ConfirmReviewAnalyzeState {
                            review_id,
                            review_title,
                        });
                    }
                    None => {
                        st.notify_info(active_t("notify.review_no_sessions"));
                    }
                }
            }
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
        st.all_repo_files = vec![
            "crates/core/src/lib.rs".to_string(),
            "src/lib.rs".to_string(),
        ];

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
        // The match counter is derived from the handshake file list, so the
        // dialog validates a launch locally without a matching RPC (c-2, c-4).
        assert_eq!(dialog.pattern_match_count(), None);
    }

    #[test]
    fn test_execute_review_finding_action_defaults_to_safe_cancel() {
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
                uuid: None,
                group: None,
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
        };
        st.review.sessions = vec![session.clone()];
        st.review.session = Some(session);

        execute_review_finding_action(&mut st, "rev-1", 1);

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
        assert_eq!(
            target.scope_title, "Finding",
            "the point form names the finding it resolves"
        );
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
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Architecture,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 2".to_string(),
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Architecture,
                    status: IssueStatus::Todo,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 2".to_string(),
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Fixed,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Bug,
                    status: IssueStatus::InProgress,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
                ReviewIssue {
                    id: 2,
                    title: "Finding 4".to_string(),
                    uuid: None,
                    group: None,
                    model: None,
                    issue_type: IssueType::Quality,
                    status: IssueStatus::Discussion,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    note: None,
                }
                .into(),
            ],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
                uuid: None,
                group: None,
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Fixed,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
    fn test_execute_all_reviews_action_includes_in_progress_findings() {
        let mut st = crate::app::test_state();
        // A finding already under implementation is as executable as a TODO one:
        // the server keeps it on the same isolated-turn path (c-1).
        st.review.sessions = vec![
            session_with_status("rev-progress", 1, IssueStatus::InProgress),
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
            "must include both the InProgress and the Todo finding"
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
                uuid: None,
                group: None,
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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
                uuid: None,
                group: None,
                model: None,
                issue_type: IssueType::Architecture,
                status: IssueStatus::Todo,
                description: String::new(),
                recommendations: Vec::new(),
                files: Vec::new(),
                readonly_files: Vec::new(),
                note: None,
            }
            .into()],
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
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

    /// The post-analysis is a costly LLM operation, so `A` arms the
    /// confirmation dialog instead of dispatching immediately: nothing reaches
    /// the daemon until the developer confirms, and the safe `Cancel` holds the
    /// default focus (c-4).
    #[tokio::test]
    async fn test_shift_a_arms_the_post_analysis_confirmation() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('A'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        let target = st
            .confirm_review_analyze
            .as_ref()
            .expect("the confirmation must be armed");
        assert_eq!(target.review_id, "rev-1");
        assert_eq!(target.review_title, "Session rev-1");
        assert_eq!(st.confirm_button, crate::app::ConfirmDialogButton::Cancel);
        assert!(
            !st.review.analyzing,
            "the classification starts only after the confirmation"
        );
        assert!(
            conn.written().is_empty(),
            "the confirmation dialog launches nothing"
        );
    }

    #[tokio::test]
    async fn test_shift_a_while_the_model_is_busy_sends_no_request() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        st.model.status = crate::views::repl::TurnStatus::Responding;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('A'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(conn.written().is_empty());
        assert!(!st.review.analyzing);
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Model is currently busy; wait for operation to finish")
        );
    }

    #[tokio::test]
    async fn test_shift_a_while_an_analysis_is_running_sends_no_request() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        st.review.analyzing = true;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('A'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(conn.written().is_empty());
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("Model is currently busy; wait for operation to finish")
        );
    }

    #[tokio::test]
    async fn test_shift_a_without_sessions_notifies_instead_of_dispatching() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        let key = KeyEvent::new(KeyCode::Char('A'), crossterm::event::KeyModifiers::NONE);
        handle_review_key(key, &state, &mut conn).await.unwrap();

        let st = state.lock().await;
        assert!(conn.written().is_empty());
        assert!(!st.review.analyzing);
        assert_eq!(
            st.active_notification().map(|n| n.text.as_str()),
            Some("No review sessions available.")
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
                        uuid: None,
                        group: None,
                        model: None,
                        issue_type: IssueType::Architecture,
                        status: IssueStatus::Todo,
                        description: String::new(),
                        recommendations: Vec::new(),
                        files: Vec::new(),
                        readonly_files: Vec::new(),
                        note: None,
                    }
                    .into()
                })
                .collect(),
            raw_markdown: String::new(),
            groups: Vec::new(),
            post_analysis: None,
            launch_config: None,
        }
    }

    /// A session whose single finding is a symptom of one architectural root
    /// cause: the group owns the root-cause plan and the finding carries none
    /// of its own (c-1, c-5).
    fn architectural_session(
        id: &str,
        subplan: Vec<tauqe_protocol::PlanItem>,
    ) -> tauqe_protocol::ReviewSession {
        use tauqe_protocol::{ReviewGroup, ReviewGroupKind};

        let mut session = todo_session(id, 1);
        session.groups = vec![ReviewGroup {
            id: "arch".to_string(),
            title: "Shared cause".to_string(),
            kind: ReviewGroupKind::Architectural,
            subplan,
            ..Default::default()
        }];
        session.items[0].group = Some("arch".to_string());
        session
    }

    /// One plan step without details, files, or children.
    fn plan_step(id: &str, title: &str) -> tauqe_protocol::PlanItem {
        tauqe_protocol::PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            ..Default::default()
        }
    }

    /// The root-cause plan the routing tests share: a container step with one
    /// child plus a second top-level step, so it carries two executable leaves.
    fn root_cause_plan() -> Vec<tauqe_protocol::PlanItem> {
        vec![
            tauqe_protocol::PlanItem {
                children: vec![plan_step("1.1", "Unify the guard")],
                ..plan_step("1", "Container step")
            },
            plan_step("2", "Reuse the guard"),
        ]
    }

    /// Presses one bare key through the Review handler.
    async fn press_review_key(
        state: &Arc<Mutex<AppState>>,
        conn: &mut RpcConnection,
        code: KeyCode,
    ) {
        handle_review_key(
            KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
            state,
            conn,
        )
        .await
        .unwrap();
    }

    /// `x` on the header of an architectural group arms the binary confirmation
    /// of its root-cause plan: one root cause is fixed by one coherent change
    /// set, so the developer never executes its findings one by one (c-1, c-4,
    /// c-5).
    #[tokio::test]
    async fn test_x_on_architectural_group_header_arms_the_root_cause_plan() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        // Row 0 is the session header and row 1 is the group header.
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the group plan confirmation must be armed");
        assert_eq!(target.review_id.as_deref(), Some("rev-arch"));
        assert_eq!(target.group_id.as_deref(), Some("arch"));
        assert_eq!(target.group_title, "Shared cause");
        assert!(target.item_id.is_none());
        assert!(target.step_id.is_none());
        assert!(
            !target.is_single_target(),
            "a whole group plan confirms as a binary card"
        );
        assert_eq!(
            target.scope_title,
            "Root-cause plan of group 'Shared cause' (2 plan step(s))"
        );
        assert_eq!(
            target.items.len(),
            1,
            "the group findings travel with its plan"
        );
        assert_eq!(
            guard.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(
            conn.written().is_empty(),
            "arming the confirmation launches nothing"
        );
    }

    /// A group whose plan the Architect has not projected yet announces the
    /// just-in-time synthesis instead of inventing a step count (c-2).
    #[tokio::test]
    async fn test_x_on_architectural_group_without_a_plan_announces_the_jit_projection() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", Vec::new())];
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the group plan confirmation must be armed");
        assert_eq!(target.group_id.as_deref(), Some("arch"));
        assert!(target.step_id.is_none());
        assert_eq!(
            target.scope_title,
            "Root-cause plan of group 'Shared cause' (not synthesized yet: projected just-in-time)"
        );
    }

    /// A simple group keeps the per-finding queue: its findings are independent
    /// defects, so each one stays its own isolated turn (c-1).
    #[tokio::test]
    async fn test_x_on_a_simple_group_header_keeps_the_per_finding_queue() {
        use tauqe_protocol::{ReviewGroup, ReviewGroupKind};

        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        let mut session = todo_session("rev-simple", 1);
        session.groups = vec![ReviewGroup {
            id: "local".to_string(),
            title: "Local Issues".to_string(),
            kind: ReviewGroupKind::Simple,
            ..Default::default()
        }];
        session.items[0].group = Some("local".to_string());
        st.review.sessions = vec![session];
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the per-finding queue must be armed");
        assert!(
            target.group_id.is_none(),
            "a simple group queues its findings one by one"
        );
        assert_eq!(target.scope_title, "Local Issues");
        assert_eq!(target.items.len(), 1);
        assert!(
            !target.whole_session,
            "a simple group queues only its own findings"
        );
    }

    /// `x` on a step of a group plan arms the point form for exactly that step,
    /// so the developer keeps the Coder choice and the directives editor
    /// (c-5).
    #[tokio::test]
    async fn test_x_on_a_group_plan_step_arms_the_single_step_form() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        // Row 3 is the first top-level step of the plan branch: the session
        // header, the group header, and the branch header precede it.
        st.review.selected_index = 3;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the step confirmation must be armed");
        assert_eq!(target.group_id.as_deref(), Some("arch"));
        assert_eq!(target.step_id.as_deref(), Some("1"));
        assert!(target.is_single_target());
        assert_eq!(
            target.scope_title,
            "Plan step 'Container step' of review group 'Shared cause'"
        );
        assert_eq!(target.items.len(), 1);
        assert_eq!(
            guard.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
    }

    /// `x` on the plan branch header is equivalent to `x` on the group header
    /// above it: both name the single root-cause plan the group owns (c-1).
    #[tokio::test]
    async fn test_x_on_a_group_plan_branch_arms_the_root_cause_plan() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        // Row 2 is the plan branch: the session header, the group header, and
        // the branch that owns the unified plan.
        st.review.selected_index = 2;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the group plan confirmation must be armed");
        assert_eq!(target.review_id.as_deref(), Some("rev-arch"));
        assert_eq!(target.group_id.as_deref(), Some("arch"));
        assert_eq!(target.group_title, "Shared cause");
        assert!(target.step_id.is_none());
        assert_eq!(
            target.scope_title,
            "Root-cause plan of group 'Shared cause' (2 plan step(s))"
        );
        assert_eq!(
            guard.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
    }

    /// `Tab` on the plan branch header folds only the branch: its steps
    /// disappear while the group header stays open and the findings of the
    /// group stay visible, so the two bands of a group fold independently
    /// (concept #c-1).
    #[tokio::test]
    async fn test_tab_on_a_group_plan_branch_folds_only_the_plan() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        // Row 2 is the plan branch: the session header and the group header
        // precede it.
        st.review.selected_index = 2;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Tab).await;

        let guard = state.lock().await;
        assert!(
            guard.review.is_group_expanded("rev-arch", "arch"),
            "folding the branch must not fold the group"
        );
        assert!(!guard.review.is_plan_branch_expanded("rev-arch", "arch"));
        let rows = guard.review.flatten_rows();
        assert!(rows.iter().any(|row| matches!(
            row,
            VisibleReviewRow::GroupPlanHeader {
                group_id,
                is_expanded: false,
                ..
            } if group_id == "arch"
        )));
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, VisibleReviewRow::SubplanItem { .. })),
            "a folded branch hides every step of its plan"
        );
        assert!(
            rows.iter()
                .any(|row| matches!(row, VisibleReviewRow::ReviewItem { item_id: 1, .. })),
            "a folded branch keeps the findings of its group visible"
        );
        assert!(
            conn.written().is_empty(),
            "folding the branch is presentation state only"
        );
        drop(guard);

        // The same chord unfolds the branch and reveals its steps again.
        press_review_key(&state, &mut conn, KeyCode::Tab).await;
        let guard = state.lock().await;
        assert!(guard.review.is_plan_branch_expanded("rev-arch", "arch"));
        let steps: Vec<String> = guard
            .review
            .flatten_rows()
            .into_iter()
            .filter_map(|row| match row {
                VisibleReviewRow::SubplanItem { step_id, .. } => Some(step_id),
                _ => None,
            })
            .collect();
        assert_eq!(steps, vec!["1".to_string(), "2".to_string()]);
    }
    /// A finding of an architectural group is never executed on its own: the
    /// handler refuses it and points at the group plan instead (c-1).
    #[tokio::test]
    async fn test_x_on_a_finding_of_an_architectural_group_is_refused() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        // Row 5 closes the group: its header, the plan branch, two plan steps,
        // and then the finding.
        st.review.selected_index = 5;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        assert!(guard.confirm_execute_review.is_none());
        assert!(conn.written().is_empty());
        assert_eq!(
            guard.active_notification().map(|n| n.text.as_str()),
            Some(
                "Finding #1 belongs to the architectural group 'Shared cause': execute the group \
                 root-cause plan from its header (x)"
            )
        );
    }

    /// The cross-session batch states its localized scope and counts the
    /// architectural root-cause plans the queue executes, whether stored or
    /// projected just-in-time (c-2, c-4).
    #[tokio::test]
    async fn test_capital_x_localizes_the_mixed_queue_scope() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![
            architectural_session("rev-arch", root_cause_plan()),
            todo_session("rev-simple", 2),
        ];

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('X')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the cross-session queue must be armed");
        assert!(target.review_id.is_none());
        assert_eq!(target.review_title, "All Review Sessions");
        assert_eq!(
            target.scope_title,
            "All remaining TODO findings — 1 architectural group plan(s) in the queue"
        );
        assert_eq!(target.items.len(), 2);
    }

    /// The header of one session queues its findings with the localized scope
    /// and states how many group plans the queue carries (c-2, c-4).
    #[tokio::test]
    async fn test_session_queue_scope_counts_the_group_plans() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        st.review.selected_index = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Char('x')).await;

        let guard = state.lock().await;
        let target = guard
            .confirm_execute_review
            .as_ref()
            .expect("the session queue must be armed");
        assert_eq!(target.review_id.as_deref(), Some("rev-arch"));
        assert!(target.group_id.is_none());
        assert!(target.item_id.is_none());
        assert_eq!(
            target.scope_title,
            "All remaining TODO findings — 1 architectural group plan(s) in the queue"
        );
        assert!(
            target.whole_session,
            "the session root queues the whole session, groups included"
        );
    }

    /// Launches the queue one bare key arms on the given sessions and returns
    /// the dispatched payload together with the launch it produced (concept
    /// #c-1).
    async fn launch_queue_with(
        code: KeyCode,
        sessions: Vec<tauqe_protocol::ReviewSession>,
        selected: usize,
    ) -> (String, ViewMode, bool, Option<String>) {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = sessions;
        st.review.selected_index = selected;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, code).await;
        assert!(
            conn.written().is_empty(),
            "arming the confirmation launches nothing"
        );

        {
            let mut guard = state.lock().await;
            guard.confirm_button = crate::app::ConfirmDialogButton::Confirm;
        }
        crate::dialogs::input::handle_dialog_event(
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .expect("in-memory dispatch never fails");

        let payload = String::from_utf8(conn.written().to_vec()).expect("utf-8 payload");
        let guard = state.lock().await;
        (
            payload,
            guard.view_mode,
            guard.model.is_batch,
            guard.model.error.clone(),
        )
    }

    /// Pressing `x` on the root of a session whose findings are symptoms of
    /// architectural groups queues that session as an implicit whole-session
    /// run: the request names the session and carries no finding list, so the
    /// server builds the group plans itself instead of refusing the launch with
    /// `ARCHITECTURAL_GROUP_EXECUTION_REQUIRED` (concept #c-1, #c-3).
    #[tokio::test]
    async fn test_x_on_the_session_root_of_an_architectural_session_omits_item_ids() {
        let (payload, view_mode, is_batch, error) = launch_queue_with(
            KeyCode::Char('x'),
            vec![architectural_session("rev-arch", root_cause_plan())],
            0,
        )
        .await;

        assert!(payload.contains("review/execute"), "{payload}");
        assert!(payload.contains("\"review_id\":\"rev-arch\""), "{payload}");
        assert!(
            !payload.contains("\"item_ids\""),
            "the implicit queue never names the symptoms of an architectural group: {payload}"
        );
        assert!(!payload.contains("\"group_id\""), "{payload}");
        assert!(!payload.contains("\"step_id\""), "{payload}");
        assert_eq!(view_mode, ViewMode::Repl);
        assert!(is_batch, "a session queue runs as a batch");
        assert!(error.is_none(), "the launch reports no protocol error");
    }

    /// The session root and the cross-session batch agree on the request shape
    /// for the same session: neither narrows the queue with a finding list, so
    /// its architectural groups are planned identically by both launches
    /// (concept #c-3).
    #[tokio::test]
    async fn test_session_root_queue_matches_the_cross_session_batch_shape() {
        let session = architectural_session("rev-arch", root_cause_plan());
        let (session_payload, _, _, _) =
            launch_queue_with(KeyCode::Char('x'), vec![session.clone()], 0).await;
        let (batch_payload, _, _, _) =
            launch_queue_with(KeyCode::Char('X'), vec![session], 0).await;

        assert!(
            session_payload.contains("\"review_id\":\"rev-arch\""),
            "{session_payload}"
        );
        assert!(
            !session_payload.contains("\"item_ids\""),
            "{session_payload}"
        );
        assert!(
            !batch_payload.contains("\"review_id\""),
            "the cross-session batch names no session: {batch_payload}"
        );
        assert!(!batch_payload.contains("\"item_ids\""), "{batch_payload}");
    }
    /// `Enter` on a group header resolves through the same routing as `x`, so
    /// the two execution chords never diverge (Conventions §4.3).
    #[tokio::test]
    async fn test_enter_on_a_group_header_arms_the_root_cause_plan() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.review.sessions = vec![architectural_session("rev-arch", root_cause_plan())];
        st.review.selected_index = 1;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());
        press_review_key(&state, &mut conn, KeyCode::Enter).await;

        let guard = state.lock().await;
        assert_eq!(
            guard
                .confirm_execute_review
                .as_ref()
                .and_then(|target| target.group_id.as_deref()),
            Some("arch")
        );
    }

    /// Proves the Review handler resolves every chord through the configured
    /// keyboard layout: a Cyrillic press navigates, executes, and refreshes
    /// exactly like its Latin equivalent (c-4).
    #[tokio::test]
    async fn cyrillic_layout_navigates_executes_and_refreshes_the_review_view() {
        let mut st = crate::app::test_state();
        st.view_mode = ViewMode::Review;
        st.tui_config.input.layout = crate::config::LayoutPreset::RuJcuken;
        st.review.sessions = vec![todo_session("rev-1", 1)];
        st.review.selected_index = 0;

        let state = Arc::new(Mutex::new(st));
        let mut conn = RpcConnection::new(Vec::<u8>::new());

        // `т` sits on `n`: two presses walk from the session header past the
        // group header onto the finding row.
        for _ in 0..2 {
            handle_review_key(
                KeyEvent::new(KeyCode::Char('т'), crossterm::event::KeyModifiers::NONE),
                &state,
                &mut conn,
            )
            .await
            .unwrap();
        }
        assert_eq!(state.lock().await.review.selected_index, 2);

        // `ч` sits on `x`: the finding under the cursor arms its confirmation.
        handle_review_key(
            KeyEvent::new(KeyCode::Char('ч'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();
        {
            let guard = state.lock().await;
            let target = guard
                .confirm_execute_review
                .as_ref()
                .expect("the execution confirmation must be armed");
            assert_eq!(target.item_id, Some(1));
        }

        // `п` sits on `g`: the view requests a lightweight refresh.
        handle_review_key(
            KeyEvent::new(KeyCode::Char('п'), crossterm::event::KeyModifiers::NONE),
            &state,
            &mut conn,
        )
        .await
        .unwrap();
        assert!(
            !conn.written().is_empty(),
            "the refresh chord must reach the daemon"
        );
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
            assert_eq!(guard.review.selected_index, 61);
            assert_cursor_visible(&mut guard.review);
        }

        // Paging back up returns one screenful with the cursor still on screen.
        handle_review_key(up, &state, &mut conn).await.unwrap();
        {
            let mut guard = state.lock().await;
            assert_eq!(guard.review.selected_index, 53);
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
