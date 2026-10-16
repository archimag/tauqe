//! Confirmation modal rendering and the shared presentation specs they use.
//!
//! Every confirmation popup exposes a `*_params` builder returning the exact
//! [`ConfirmModalParams`] used for rendering. Mouse hit-testing reuses the same
//! builders, so the action-button geometry derived from these parameters is
//! identical for rendering and hit-testing and cannot drift (Conventions.md §7).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{AppState, BatchCancelButton};
use crate::theme::engine::current_engine;
use crate::ui::header::truncate_to_width;

use super::{render_modal_confirm, ConfirmModalParams, ModalTone};

/// Presentation parameters for the interruption confirmation, shared by the
/// renderer and mouse hit-testing.
pub fn confirm_cancel_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Interrupt active model operation?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from("Generation will be stopped immediately."),
        Line::from(Span::styled(
            "All in-progress edits and uncommitted diffs will be discarded.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Interruption",
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: "Interrupt (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_cancel_params(state));
}

/// Presentation parameters for the quit confirmation.
pub fn confirm_quit_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let activity = if state.review.running {
        "Code review is currently in progress."
    } else if state.model.is_busy() {
        "Model generation / turn is currently in progress."
    } else if !state.input_editor.is_empty() {
        "You have an unsubmitted prompt draft in the editor."
    } else if state.squash_dialog.as_ref().is_some_and(|d| d.applying) {
        "Squash operation is currently being applied."
    } else if state.squash_dialog.as_ref().is_some_and(|d| !d.message_editor.is_empty()) {
        "You have an uncommitted squash message in progress."
    } else {
        "Active session work may be lost."
    };

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Quit TAUQE?",
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        Line::from(activity),
        Line::from(Span::styled(
            "Exiting now will terminate the session and discard active work.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Quit",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Quit TAUQE (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_quit_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_quit_params(state));
}

/// Presentation parameters for the undo confirmation.
pub fn confirm_undo_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to undo the last AI commit?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
    ];

    if let Some(hash) = &state.model.last_commit_hash {
        body_lines.push(Line::from(vec![
            Span::raw("Commit: "),
            Span::styled(hash.clone(), engine.fg(palette.status_info).bold()),
        ]));
    }
    if let Some(summary) = &state.model.last_commit_summary {
        body_lines.push(Line::from(vec![
            Span::raw("Summary: "),
            Span::styled(summary.clone(), engine.fg(palette.text_primary)),
        ]));
    }
    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "Any uncommitted changes from the pre-edit checkpoint will be restored.",
        engine.fg(palette.text_muted),
    )));

    ConfirmModalParams {
        title: "Confirm Undo",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Confirm Undo (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_undo_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_undo_params(state));
}

/// Presentation parameters for the discussion-focus detach confirmation.
///
/// Detaching is non-destructive to the working tree, so the confirmation
/// guards against accidental loss of conversational context rather than data:
/// the safe `Cancel` default keeps the active plan or review anchor in place.
pub fn confirm_detach_focus_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Detach the active discussion focus?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from("The REPL will return to the free codebase context."),
        Line::from(Span::styled(
            "The plan or review anchor is released until you focus on it again.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Detach Focus",
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: "Detach (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: false,
        width: 58,
    }
}

pub fn render_confirm_detach_focus_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_detach_focus_params(state));
}

/// Presentation parameters for the review-execution confirmation.
pub fn execute_review_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_execute_review.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let is_all_sessions = target.review_id.is_none();
    let is_single = target.items.len() == 1 && !is_all_sessions;
    let title = if is_all_sessions {
        "Execute All Review Sessions"
    } else if is_single {
        "Execute Review Finding"
    } else {
        "Execute Review Session"
    };

    let heading = if is_all_sessions {
        "Resolve remaining findings sequentially across all sessions with automated verification?"
    } else if is_single {
        "Resolve target review finding autonomously?"
    } else {
        "Resolve remaining findings sequentially with automated verification?"
    };

    let review_line = if let Some(ref rev_id) = target.review_id {
        Line::from(vec![
            Span::styled(" Review: ", engine.fg(palette.text_muted)),
            Span::styled(&target.review_title, engine.fg(palette.text_primary).bold()),
            Span::styled(format!(" [{}]", rev_id), engine.fg(palette.text_muted)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Review: ", engine.fg(palette.text_muted)),
            Span::styled(&target.review_title, engine.fg(palette.text_primary).bold()),
        ])
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            heading,
            engine.fg(palette.status_info).bold(),
        )),
        Line::raw(""),
        review_line,
        Line::from(vec![
            Span::styled(" Scope: ", engine.fg(palette.text_muted)),
            Span::styled(&target.scope_title, engine.fg(palette.status_warning).bold()),
            Span::styled(format!(" ({} finding(s) to resolve)", target.items.len()), engine.fg(palette.text_primary)),
        ]),
    ];

    if is_single {
        let item = &target.items[0];
        if !item.files.is_empty() {
            let loc = item.files.join(", ");
            body_lines.push(Line::from(vec![
                Span::styled(" Files: ", engine.fg(palette.text_muted)),
                Span::styled(loc, engine.fg(palette.status_info)),
            ]));
        }
        let trimmed = item.body.trim();
        if !trimmed.is_empty() {
            let first_line = trimmed.lines().next().unwrap_or("");
            body_lines.push(Line::from(vec![
                Span::styled(" Recommendation: ", engine.fg(palette.text_muted)),
                Span::styled(
                    truncate_to_width(first_line, 45),
                    engine.fg(palette.text_secondary),
                ),
            ]));
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(" Findings in execution queue:", engine.fg(palette.text_muted))));
        for (i, item) in target.items.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), engine.fg(palette.status_info)),
                Span::styled(format!("[{}] #{} {}", item.severity, item.id, truncate_to_width(&item.title, 36)), engine.fg(palette.text_primary)),
            ]));
        }
        if target.items.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                format!("   ... and {} more finding(s)", target.items.len() - 4),
                engine.fg(palette.text_muted).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE will resolve each finding in an isolated turn with compiler verification gates.",
        engine.fg(palette.text_muted),
    )));
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            "Fail-Fast Policy: execution stops and rolls back immediately if any finding fails verification.",
            engine.fg(palette.text_muted),
        )));
    }

    let confirm_label = if is_single {
        "Resolve (Enter / Y)"
    } else {
        "Resolve Queue (Enter / Y)"
    };

    Some(ConfirmModalParams {
        title,
        tone: ModalTone::Info,
        body_lines,
        confirm_label,
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: false,
        width: 66,
    })
}

pub fn render_confirm_execute_review_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = execute_review_params(state) {
        render_modal_confirm(frame, params);
    }
}

/// Presentation parameters for the plan-step execution confirmation.
pub fn execute_scope_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_execute_scope.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let is_single = target.steps.len() == 1;
    let title = if is_single {
        "Execute Plan Step"
    } else {
        "Execute Plan Scope"
    };

    let heading = if is_single {
        "Execute target plan step autonomously?"
    } else {
        "Execute plan steps sequentially with automated verification?"
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            heading,
            engine.fg(palette.status_info).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(" Plan: ", engine.fg(palette.text_muted)),
            Span::styled(&target.plan_title, engine.fg(palette.text_primary).bold()),
            Span::styled(format!(" [{}]", target.plan_id), engine.fg(palette.text_muted)),
        ]),
        Line::from(vec![
            Span::styled(" Scope: ", engine.fg(palette.text_muted)),
            Span::styled(&target.scope_title, engine.fg(palette.status_warning).bold()),
            Span::styled(format!(" ({} step(s) to execute)", target.steps.len()), engine.fg(palette.text_primary)),
        ]),
    ];

    if is_single {
        let step = &target.steps[0];
        if let Some(details) = &step.details {
            let trimmed = details.trim();
            if !trimmed.is_empty() {
                let first_line = trimmed.lines().next().unwrap_or("");
                body_lines.push(Line::from(vec![
                    Span::styled(" Details: ", engine.fg(palette.text_muted)),
                    Span::styled(
                        truncate_to_width(first_line, 45),
                        engine.fg(palette.text_secondary),
                    ),
                ]));
            }
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(" Steps in execution queue:", engine.fg(palette.text_muted))));
        for (i, step) in target.steps.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), engine.fg(palette.status_info)),
                Span::styled(format!("#{} {}", step.id, truncate_to_width(&step.title, 40)), engine.fg(palette.text_primary)),
            ]));
        }
        if target.steps.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                format!("   ... and {} more step(s)", target.steps.len() - 4),
                engine.fg(palette.text_muted).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        "TAUQE will execute each step in an isolated turn with compiler verification gates.",
        engine.fg(palette.text_muted),
    )));
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            "Execution stops immediately if any step fails or is interrupted.",
            engine.fg(palette.text_muted),
        )));
    }

    let confirm_label = if is_single {
        "Execute (Enter / Y)"
    } else {
        "Execute Scope (Enter / Y)"
    };

    Some(ConfirmModalParams {
        title,
        tone: ModalTone::Info,
        body_lines,
        confirm_label,
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: false,
        width: 66,
    })
}

pub fn render_confirm_execute_scope_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = execute_scope_params(state) {
        render_modal_confirm(frame, params);
    }
}

/// Presentation parameters for the plan-deletion confirmation.
pub fn delete_plan_params<'a>(state: &'a AppState, plan_id: &str) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let plan_title = state
        .plans_view
        .plans
        .iter()
        .find(|p| p.id == plan_id)
        .map(|p| p.title.clone())
        .unwrap_or_else(|| plan_id.to_string());

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to delete this plan?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Plan: "),
            Span::styled(format!("{} ({})", plan_title, plan_id), engine.fg(palette.status_info).bold()),
        ]),
        Line::from(Span::styled(
            "The plan file in .tauqe/plans/ will be permanently removed.",
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Delete Plan",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Confirm Delete (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_delete_plan_popup(frame: &mut ratatui::Frame, plan_id: &str, state: &AppState) {
    render_modal_confirm(frame, delete_plan_params(state, plan_id));
}

/// Presentation parameters for the review-deletion confirmation.
pub fn delete_review_params<'a>(state: &'a AppState, session_id: &str) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let review_title = state
        .review
        .sessions
        .iter()
        .find(|s| s.id == session_id)
        .map(|s| s.title.clone())
        .or_else(|| state.review.session.as_ref().filter(|s| s.id == session_id).map(|s| s.title.clone()))
        .unwrap_or_else(|| session_id.to_string());

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to delete this review session?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Review: "),
            Span::styled(format!("{} ({})", review_title, session_id), engine.fg(palette.status_info).bold()),
        ]),
        Line::from(Span::styled(
            "The review file in .tauqe/reviews/ will be permanently removed.",
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Delete Review",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Confirm Delete (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_delete_review_popup(frame: &mut ratatui::Frame, session_id: &str, state: &AppState) {
    render_modal_confirm(frame, delete_review_params(state, session_id));
}

/// Presentation parameters for the "clear Auto context" confirmation.
pub fn clear_auto_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let auto_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == tauqe_protocol::ContextLayer::Auto)
        .count();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Clear all Auto-context files?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Are you sure you want to remove all "),
            Span::styled(
                format!("{} file(s)", auto_count),
                engine.fg(palette.button_active_bg).bold(),
            ),
            Span::raw(" from the Auto layer?"),
        ]),
        Line::from(Span::styled(
            "Persistent User files and Pinned files will not be affected.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Clear Auto Context",
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: "Confirm Clear (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: false,
        width: 58,
    }
}

pub fn render_confirm_clear_auto_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, clear_auto_params(state));
}

/// Presentation parameters for the "clear User context" confirmation. Mirrors the
/// Auto-layer reset: a guarded confirmation whose safe default is `Cancel`, while
/// the protected Pinned and regenerable Auto layers stay untouched.
pub fn clear_user_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let user_count = state
        .context
        .items
        .iter()
        .filter(|i| i.layer == tauqe_protocol::ContextLayer::User)
        .count();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Clear all User-context files?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Are you sure you want to remove all "),
            Span::styled(
                format!("{} file(s)", user_count),
                engine.fg(palette.button_active_bg).bold(),
            ),
            Span::raw(" from the User layer?"),
        ]),
        Line::from(Span::styled(
            "Protected Pinned files and Auto-requested files will not be affected.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Clear User Context",
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: "Confirm Clear (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: false,
        width: 58,
    }
}

pub fn render_confirm_clear_user_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, clear_user_params(state));
}

/// Presentation parameters for the "clear history" confirmation.
pub fn clear_history_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Are you sure you want to clear conversation history?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from("This will permanently erase session history:"),
        Line::from(Span::styled(
            "  .tauqe/history.jsonl",
            engine.fg(palette.text_muted).italic(),
        )),
        Line::from(Span::styled(
            "and clear the current model view output.",
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: "Confirm Clear History",
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: "Confirm Clear (Y)",
        cancel_label: "Cancel (Esc)",
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, clear_history_params(state));
}

/// Action-button labels of the batch-cancellation dialog, in left-to-right
/// order. The geometry and the renderer derive their layout from this single
/// list so the drawn buttons are always the clickable ones.
const BATCH_CANCEL_LABELS: [&str; 3] = [" Continue (C) ", " Soft Stop (S) ", " Hard Stop (H) "];
const BATCH_CANCEL_WIDTH: u16 = 64;
const BATCH_CANCEL_GAP: u16 = 2;

/// Body content of the batch-cancellation dialog.
///
/// The three actions are described explicitly so the choice between keeping the
/// queue, stopping after the current member, and aborting immediately is never
/// ambiguous (Conventions.md §7).
pub fn batch_cancel_body_lines() -> Vec<Line<'static>> {
    let engine = current_engine();
    let palette = engine.palette();
    vec![
        Line::raw(""),
        Line::from(Span::styled(
            "Cancel batch execution?",
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from("Continue keeps the batch running."),
        Line::from("Soft Stop finishes the current step, then stops the queue."),
        Line::from(Span::styled(
            "Hard Stop aborts the current step immediately.",
            engine.fg(palette.text_muted),
        )),
    ]
}

/// Exact geometry and action-button bounds of the batch-cancellation dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchCancelModalGeometry {
    pub area: Rect,
    pub continue_button: Rect,
    pub soft_stop_button: Rect,
    pub hard_stop_button: Rect,
}

/// Computes the geometry and button bounds of the batch-cancellation dialog so
/// that rendering and mouse hit-testing agree on the same layout.
pub fn batch_cancel_modal_geometry(term: Rect) -> BatchCancelModalGeometry {
    let body_lines_count = batch_cancel_body_lines().len();
    let needed_height = (body_lines_count as u16).saturating_add(7).max(10);
    let height_percent = (needed_height * 100 / term.height.max(1)).clamp(25, 90);
    let area = crate::ui::centered_rect(BATCH_CANCEL_WIDTH, height_percent, term);

    // Mirror the renderer's own split (flexible body, two-row action zone,
    // one-row docked hint strip) so hit-testing lands on the row the buttons
    // are drawn in and can never drift from the rendered layout (Conventions §6, §7).
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);
    let action_bar = chunks[1];
    let btn_y = action_bar.y;

    let widths: [u16; 3] = [
        BATCH_CANCEL_LABELS[0].chars().count() as u16,
        BATCH_CANCEL_LABELS[1].chars().count() as u16,
        BATCH_CANCEL_LABELS[2].chars().count() as u16,
    ];
    let total = widths[0] + widths[1] + widths[2] + BATCH_CANCEL_GAP * 2;
    let start = action_bar.x + action_bar.width.saturating_sub(total) / 2;

    let continue_button = Rect::new(start, btn_y, widths[0], 1);
    let soft_x = start + widths[0] + BATCH_CANCEL_GAP;
    let soft_stop_button = Rect::new(soft_x, btn_y, widths[1], 1);
    let hard_x = soft_x + widths[1] + BATCH_CANCEL_GAP;
    let hard_stop_button = Rect::new(hard_x, btn_y, widths[2], 1);

    BatchCancelModalGeometry {
        area,
        continue_button,
        soft_stop_button,
        hard_stop_button,
    }
}

/// Footer hint of the batch-cancellation dialog.
///
/// Navigation and dismissal are the shared dialog vocabulary, projected through
/// the configured modifiers; the fast-select verbs are the batch's own
/// `Continue` / `Soft Stop` / `Hard Stop` accelerators, so they are named here
/// rather than in the universal dialog map.
fn batch_cancel_hint() -> String {
    format!(
        "{} • s / h / c fast select • {}",
        crate::components::modal::dialog_navigation_hint(),
        crate::components::modal::dialog_dismiss_hint(),
    )
}

/// Renders the three-way batch-cancellation dialog.
pub fn render_confirm_batch_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let engine = current_engine();
    let palette = engine.palette();
    let geom = batch_cancel_modal_geometry(frame.area());

    let inner_area = crate::components::modal::render_modal_container(
        frame,
        geom.area,
        "Cancel Batch Execution",
        ModalTone::Warning,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner_area);

    let body = Paragraph::new(batch_cancel_body_lines())
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, chunks[0]);

    let focused = state.batch_cancel_button;
    let button_style = |button: BatchCancelButton| {
        if button == focused {
            engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold()
        } else {
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg))
        }
    };

    let actions_line = Line::from(vec![
        Span::styled(
            BATCH_CANCEL_LABELS[0],
            button_style(BatchCancelButton::Continue),
        ),
        Span::raw("  "),
        Span::styled(
            BATCH_CANCEL_LABELS[1],
            button_style(BatchCancelButton::SoftStop),
        ),
        Span::raw("  "),
        Span::styled(
            BATCH_CANCEL_LABELS[2],
            button_style(BatchCancelButton::HardStop),
        ),
    ]);
    let actions_widget = Paragraph::new(actions_line).alignment(Alignment::Center);
    frame.render_widget(actions_widget, chunks[1]);

    crate::components::modal::render_dialog_footer(frame, chunks[2], &batch_cancel_hint());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    /// The geometry used for mouse hit-testing must land exactly on the drawn
    /// action buttons, regardless of label length or body line count.
    #[test]
    fn rendered_confirm_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let params = ConfirmModalParams {
            title: "Confirm Test",
            tone: ModalTone::Dangerous,
            body_lines: vec![Line::raw(""), Line::from("Body"), Line::raw("")],
            confirm_label: "Resolve (Enter)",
            cancel_label: "Cancel (Esc)",
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            destructive: true,
            width: 66,
        };
        let body_count = params.body_lines.len();

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_modal_confirm(f, params))
            .unwrap();

        let geom = crate::components::confirm::confirm_modal_geometry(
            term,
            66,
            body_count,
            "Resolve (Enter)",
            "Cancel (Esc)",
        );
        let buffer = terminal.backend().buffer();

        let confirm_text: String = (geom.confirm_button.x..geom.confirm_button.x + geom.confirm_button.width)
            .map(|x| buffer[(x, geom.confirm_button.y)].symbol())
            .collect();
        assert!(
            confirm_text.contains("Resolve"),
            "confirm row text: {confirm_text:?}"
        );

        let cancel_text: String = (geom.cancel_button.x..geom.cancel_button.x + geom.cancel_button.width)
            .map(|x| buffer[(x, geom.cancel_button.y)].symbol())
            .collect();
        assert!(
            cancel_text.contains("Cancel"),
            "cancel row text: {cancel_text:?}"
        );
    }

    #[test]
    fn test_execute_review_params_all_sessions() {
        let mut st = crate::app::test_state();
        st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState {
            review_id: None,
            review_title: "All Review Sessions".to_string(),
            scope_title: "All remaining TODO findings across all sessions (2 items)".to_string(),
            item_id: None,
            items: vec![
                tauqe_protocol::ReviewItem {
                    id: 1,
                    title: "First".to_string(),
                    model: None,
                    severity: tauqe_protocol::ReviewSeverity::Warning,
                    status: tauqe_protocol::ReviewItemStatus::Todo,
                    files: vec![],
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
                tauqe_protocol::ReviewItem {
                    id: 2,
                    title: "Second".to_string(),
                    model: None,
                    severity: tauqe_protocol::ReviewSeverity::Suggestion,
                    status: tauqe_protocol::ReviewItemStatus::Todo,
                    files: vec![],
                    subplan: Vec::new(),
                    resolution_notes: None,
                    body: String::new(),
                },
            ],
        });

        let params = execute_review_params(&st).expect("params should be generated");
        assert_eq!(params.title, "Execute All Review Sessions");
        assert_eq!(params.confirm_button, crate::app::ConfirmDialogButton::Cancel);
        assert!(!params.destructive);
        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Review: All Review Sessions")));
        assert!(texts.iter().any(|t| t.contains("2 finding(s) to resolve")));
    }

    #[test]
    fn batch_cancel_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        let mut state = crate::app::test_state();
        state.confirm_batch_cancel = true;

        terminal
            .draw(|f| render_confirm_batch_cancel_popup(f, &state))
            .unwrap();

        let geom = batch_cancel_modal_geometry(term);
        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };

        assert!(read(geom.continue_button).contains("Continue"));
        assert!(read(geom.soft_stop_button).contains("Soft Stop"));
        assert!(read(geom.hard_stop_button).contains("Hard Stop"));
    }
}
