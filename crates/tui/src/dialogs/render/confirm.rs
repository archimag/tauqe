//! Confirmation modal rendering and the shared presentation specs they use.
//!
//! Every confirmation popup exposes a `*_params` builder returning the exact
//! [`ConfirmModalParams`] used for rendering. Mouse hit-testing reuses the same
//! builders, so the action-button geometry derived from these parameters is
//! identical for rendering and hit-testing and cannot drift (Conventions.md §7).
//!
//! Every visible phrase — title, question, body, and button verb — is resolved
//! through the runtime locale (`crate::i18n::active_t`), so a hot-swapped locale
//! reaches the whole dialog without touching the renderers (Conventions §9, c-1).

use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use crate::app::AppState;
use crate::i18n::active_t;
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
            active_t("dialog.confirm_cancel.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::raw(active_t("dialog.confirm_cancel.will_stop")),
        Line::from(Span::styled(
            active_t("dialog.confirm_cancel.discard_note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.confirm_cancel.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.interrupt"),
        cancel_label: active_t("modal.cancel"),
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
        active_t("dialog.confirm_quit.activity_review")
    } else if state.model.is_busy() {
        active_t("dialog.confirm_quit.activity_model")
    } else if !state.input_editor.is_empty() {
        active_t("dialog.confirm_quit.activity_draft")
    } else if state.squash_dialog.as_ref().is_some_and(|d| d.applying) {
        active_t("dialog.confirm_quit.activity_squash_applying")
    } else if state
        .squash_dialog
        .as_ref()
        .is_some_and(|d| !d.message_editor.is_empty())
    {
        active_t("dialog.confirm_quit.activity_squash_message")
    } else {
        active_t("dialog.confirm_quit.activity_default")
    };

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.confirm_quit.question"),
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        Line::raw(activity),
        Line::from(Span::styled(
            active_t("dialog.confirm_quit.exit_note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.confirm_quit.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.quit"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_quit_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_quit_params(state));
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
            active_t("dialog.confirm_detach_focus.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::raw(active_t("dialog.confirm_detach_focus.repl_note")),
        Line::from(Span::styled(
            active_t("dialog.confirm_detach_focus.anchor_note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.confirm_detach_focus.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.detach"),
        cancel_label: active_t("modal.cancel"),
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
        active_t("dialog.execute_review.title_all")
    } else if is_single {
        active_t("dialog.execute_review.title_single")
    } else {
        active_t("dialog.execute_review.title_session")
    };

    let heading = if is_all_sessions {
        active_t("dialog.execute_review.heading_all")
    } else if is_single {
        active_t("dialog.execute_review.heading_single")
    } else {
        active_t("dialog.execute_review.heading_session")
    };

    let review_line = if let Some(ref rev_id) = target.review_id {
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_review.review_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(&target.review_title, engine.fg(palette.text_primary).bold()),
            Span::styled(format!(" [{}]", rev_id), engine.fg(palette.text_muted)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_review.review_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(&target.review_title, engine.fg(palette.text_primary).bold()),
        ])
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(heading, engine.fg(palette.status_info).bold())),
        Line::raw(""),
        review_line,
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_review.scope_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                &target.scope_title,
                engine.fg(palette.status_warning).bold(),
            ),
            Span::styled(
                active_t("dialog.execute_review.scope_count").replacen(
                    "{}",
                    &target.items.len().to_string(),
                    1,
                ),
                engine.fg(palette.text_primary),
            ),
        ]),
    ];

    if is_single {
        let item = &target.items[0];
        if !item.files.is_empty() {
            let loc = item.files.join(", ");
            body_lines.push(Line::from(vec![
                Span::styled(
                    active_t("dialog.execute_review.files_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(loc, engine.fg(palette.status_info)),
            ]));
        }
        if !item.readonly_files.is_empty() {
            body_lines.push(Line::from(vec![
                Span::styled(
                    active_t("dialog.execute_review.readonly_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(
                    item.readonly_files.join(", "),
                    engine.fg(palette.status_warning),
                ),
            ]));
        }
        let trimmed = item.description.trim();
        if !trimmed.is_empty() {
            let first_line = trimmed.lines().next().unwrap_or("");
            body_lines.push(Line::from(vec![
                Span::styled(
                    active_t("dialog.execute_review.description_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(
                    truncate_to_width(first_line, 45),
                    engine.fg(palette.text_secondary),
                ),
            ]));
        }
        if let Some(first) = item.recommendations.first() {
            body_lines.push(Line::from(vec![
                Span::styled(
                    active_t("dialog.execute_review.recommendation_label"),
                    engine.fg(palette.text_muted),
                ),
                Span::styled(
                    truncate_to_width(first, 45),
                    engine.fg(palette.text_secondary),
                ),
            ]));
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_review.queue_header"),
            engine.fg(palette.text_muted),
        )));
        for (i, item) in target.items.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), engine.fg(palette.status_info)),
                Span::styled(
                    format!(
                        "[{}] #{} {}",
                        item.issue_type,
                        item.id,
                        truncate_to_width(&item.title, 36)
                    ),
                    engine.fg(palette.text_primary),
                ),
            ]));
        }
        if target.items.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                active_t("dialog.execute_review.queue_more").replacen(
                    "{}",
                    &(target.items.len() - 4).to_string(),
                    1,
                ),
                engine.fg(palette.text_muted).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.execute_review.note"),
        engine.fg(palette.text_muted),
    )));
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_review.fail_fast"),
            engine.fg(palette.text_muted),
        )));
    }

    let confirm_label = if is_single {
        active_t("dialog.button.resolve")
    } else {
        active_t("dialog.button.resolve_queue")
    };

    Some(ConfirmModalParams {
        title,
        tone: ModalTone::Info,
        body_lines,
        confirm_label,
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: false,
        width: 66,
    })
}

pub fn render_confirm_execute_review_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if state
        .confirm_execute_review
        .as_ref()
        .is_some_and(|target| target.is_single_target())
    {
        super::execute_review::render_execute_review_dialog(frame, state);
        return;
    }
    if let Some(params) = execute_review_params(state) {
        render_modal_confirm(frame, params);
    }
}

pub fn render_confirm_execute_scope_popup(frame: &mut ratatui::Frame, state: &AppState) {
    super::execute_scope::render_execute_scope_dialog(frame, state);
}

/// Presentation parameters for the multi-plan execution pipeline confirmation.
///
/// The confirmation names the whole queue and the total queued step count before
/// any bounded turn spends tokens, because a pipeline keeps the operation slot
/// for every plan it runs and stops the queue on the first failure
/// (Conventions §5.4).
pub fn execute_pipeline_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_execute_pipeline.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.execute_pipeline.heading"),
            engine.fg(palette.status_info).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_pipeline.plans_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                active_t("dialog.execute_pipeline.plans_count").replacen(
                    "{}",
                    &target.plan_titles.len().to_string(),
                    1,
                ),
                engine.fg(palette.status_warning).bold(),
            ),
            Span::styled(
                active_t("dialog.execute_pipeline.steps_count").replacen(
                    "{}",
                    &target.steps.len().to_string(),
                    1,
                ),
                engine.fg(palette.text_primary),
            ),
        ]),
    ];

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.execute_pipeline.queue_header"),
        engine.fg(palette.text_muted),
    )));
    for (i, title) in target.plan_titles.iter().take(4).enumerate() {
        body_lines.push(Line::from(vec![
            Span::styled(format!("   {}. ", i + 1), engine.fg(palette.status_info)),
            Span::styled(
                truncate_to_width(title, 44),
                engine.fg(palette.text_primary),
            ),
        ]));
    }
    if target.plan_titles.len() > 4 {
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_pipeline.queue_more").replacen(
                "{}",
                &(target.plan_titles.len() - 4).to_string(),
                1,
            ),
            engine.fg(palette.text_muted).italic(),
        )));
    }

    if !target.pending_synthesis.is_empty() {
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_pipeline.jit_note").replacen(
                "{}",
                &target.pending_synthesis.len().to_string(),
                1,
            ),
            engine.fg(palette.text_secondary),
        )));
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.execute_pipeline.note"),
        engine.fg(palette.text_muted),
    )));

    Some(ConfirmModalParams {
        title: active_t("dialog.execute_pipeline.title"),
        tone: ModalTone::Info,
        body_lines,
        confirm_label: active_t("dialog.button.execute_pipeline"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: false,
        width: 68,
    })
}

pub fn render_confirm_execute_pipeline_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = execute_pipeline_params(state) {
        render_modal_confirm(frame, params);
    }
}

/// Number of implementation leaves of a plan that are not `Done`.
///
/// The metric is the authoritative leaf accounting of `Plan::count_stats`, which
/// already excludes cancelled leaves from the total, so a plan whose leaves are
/// all completed or cancelled needs no escalation (c-1).
pub(crate) fn plan_unfinished_steps(plan: &tauqe_protocol::Plan) -> usize {
    let (total, completed) = plan.count_stats();
    total.saturating_sub(completed)
}

/// Width of the single-tier plan-deletion confirmation.
const DELETE_PLAN_WIDTH: u16 = 58;
/// Width of the two-tier plan-deletion confirmation.
const DELETE_PLAN_FINAL_WIDTH: u16 = 64;

/// Title of the plan a dialog targets, resolved from the loaded plans.
///
/// The lookup falls back to the raw identifier so the dialog always names its
/// target even when the plan list has not been refreshed (c-2).
fn plan_display_title(state: &AppState, plan_id: &str) -> String {
    state
        .plans_view
        .plans
        .iter()
        .find(|p| p.id == plan_id)
        .map(|p| p.title.clone())
        .unwrap_or_else(|| plan_id.to_string())
}

/// Plan field shared by both plan-deletion confirmations:
/// `Plan: <title> (<id>)`.
fn delete_plan_line(state: &AppState, plan_id: &str) -> Line<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Line::from(vec![
        Span::raw(active_t("dialog.delete_plan.plan_label")),
        Span::styled(
            format!("{} ({})", plan_display_title(state, plan_id), plan_id),
            engine.fg(palette.status_info).bold(),
        ),
    ])
}

/// Presentation parameters for the first tier of the plan-deletion confirmation.
///
/// The dialog states the derived plan status together with how much unfinished
/// work the deletion would discard, so the cost is visible before confirming.
/// The safe `Cancel` button holds the default focus (Conventions §5.4, c-1).
pub fn delete_plan_params<'a>(state: &'a AppState, plan_id: &str) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let plan = state.plans_view.plans.iter().find(|p| p.id == plan_id);
    let status = plan.map(|p| p.status()).unwrap_or_default();
    let unfinished = plan.map(plan_unfinished_steps).unwrap_or(0);
    let remaining = if unfinished == 0 {
        active_t("dialog.delete_plan.all_completed")
    } else {
        active_t("dialog.delete_plan.unfinished_count").replacen("{}", &unfinished.to_string(), 1)
    };

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.delete_plan.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        delete_plan_line(state, plan_id),
        Line::from(vec![
            Span::styled(
                active_t("dialog.delete_plan.status_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(status.to_string(), engine.fg(palette.status_info).bold()),
        ]),
        Line::from(vec![
            Span::styled(
                active_t("dialog.delete_plan.unfinished_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                remaining,
                if unfinished == 0 {
                    engine.fg(palette.text_secondary)
                } else {
                    engine.fg(palette.status_warning).bold()
                },
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.delete_plan.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.delete_plan.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: DELETE_PLAN_WIDTH,
    }
}

/// Presentation parameters for the second tier of the plan-deletion
/// confirmation.
///
/// A plan whose leaves are still pending, in progress, or blocked carries task
/// context that no Git commit restores, so the second tier names the remaining
/// work explicitly and warns that the recorded progress is lost for good (c-1).
pub fn delete_plan_final_params<'a>(state: &'a AppState, plan_id: &str) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();
    let plan = state.plans_view.plans.iter().find(|p| p.id == plan_id);
    let (total, completed) = plan.map(|p| p.count_stats()).unwrap_or((0, 0));
    let unfinished = total.saturating_sub(completed);

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.delete_plan_final.question"),
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        delete_plan_line(state, plan_id),
        Line::from(vec![
            Span::styled(
                active_t("dialog.delete_plan.unfinished_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                active_t("dialog.delete_plan_final.progress")
                    .replacen("{}", &unfinished.to_string(), 1)
                    .replacen("{}", &total.to_string(), 1),
                engine.fg(palette.status_error).bold(),
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.delete_plan_final.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.delete_plan_final.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.permanently_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: DELETE_PLAN_FINAL_WIDTH,
    }
}

pub fn render_confirm_delete_plan_popup(
    frame: &mut ratatui::Frame,
    plan_id: &str,
    state: &AppState,
) {
    render_modal_confirm(frame, delete_plan_params(state, plan_id));
}

pub fn render_confirm_delete_plan_final_popup(
    frame: &mut ratatui::Frame,
    plan_id: &str,
    state: &AppState,
) {
    render_modal_confirm(frame, delete_plan_final_params(state, plan_id));
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
        .or_else(|| {
            state
                .review
                .session
                .as_ref()
                .filter(|s| s.id == session_id)
                .map(|s| s.title.clone())
        })
        .unwrap_or_else(|| session_id.to_string());

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.delete_review.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw(active_t("dialog.delete_review.review_label")),
            Span::styled(
                format!("{} ({})", review_title, session_id),
                engine.fg(palette.status_info).bold(),
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.delete_review.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.delete_review.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_delete_review_popup(
    frame: &mut ratatui::Frame,
    session_id: &str,
    state: &AppState,
) {
    render_modal_confirm(frame, delete_review_params(state, session_id));
}
/// Presentation parameters for the Architect post-analysis confirmation.
///
/// The classification is a costly LLM layer over immutable findings, so the
/// dialog names the audited session and leaves the safe `Cancel` button in
/// focus before any tokens are spent (c-4).
pub fn review_analyze_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_review_analyze.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.review_analyze.heading"),
            engine.fg(palette.status_info).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.review_analyze.review_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(&target.review_title, engine.fg(palette.text_primary).bold()),
            Span::styled(
                format!(" [{}]", target.review_id),
                engine.fg(palette.text_muted),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.review_analyze.note"),
            engine.fg(palette.text_muted),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.review_analyze.title"),
        tone: ModalTone::Info,
        body_lines,
        confirm_label: active_t("dialog.button.run_analysis"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: false,
        width: 64,
    })
}

pub fn render_confirm_review_analyze_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = review_analyze_params(state) {
        render_modal_confirm(frame, params);
    }
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
            active_t("dialog.clear_auto.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw(active_t("dialog.clear_auto.body_prefix")),
            Span::styled(
                active_t("dialog.clear_auto.body_count").replacen("{}", &auto_count.to_string(), 1),
                engine.fg(palette.button_active_bg).bold(),
            ),
            Span::raw(active_t("dialog.clear_auto.body_suffix")),
        ]),
        Line::from(Span::styled(
            active_t("dialog.clear_auto.note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.clear_auto.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_clear"),
        cancel_label: active_t("modal.cancel"),
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
            active_t("dialog.clear_user.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw(active_t("dialog.clear_user.body_prefix")),
            Span::styled(
                active_t("dialog.clear_user.body_count").replacen("{}", &user_count.to_string(), 1),
                engine.fg(palette.button_active_bg).bold(),
            ),
            Span::raw(active_t("dialog.clear_user.body_suffix")),
        ]),
        Line::from(Span::styled(
            active_t("dialog.clear_user.note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.clear_user.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_clear"),
        cancel_label: active_t("modal.cancel"),
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
            active_t("dialog.clear_history.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::raw(active_t("dialog.clear_history.erase_note")),
        Line::from(Span::styled(
            "  .tauqe/history.jsonl",
            engine.fg(palette.text_muted).italic(),
        )),
        Line::from(Span::styled(
            active_t("dialog.clear_history.view_note"),
            engine.fg(palette.text_muted),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.clear_history.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_clear"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_clear_history_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, clear_history_params(state));
}

/// Width of the single-tier clear-horizon confirmation.
const CLEAR_HORIZON_WIDTH: u16 = 62;
/// Width of the two-tier clear-horizon confirmation.
const CLEAR_HORIZON_FINAL_WIDTH: u16 = 64;

/// Plan field of both clear-horizon confirmations: `Plan: <title> (<id>)`.
fn clear_horizon_plan_line(state: &AppState, plan_id: &str) -> Line<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Line::from(vec![
        Span::raw(active_t("dialog.delete_plan.plan_label")),
        Span::styled(
            format!("{} ({})", plan_display_title(state, plan_id), plan_id),
            engine.fg(palette.status_info).bold(),
        ),
    ])
}

/// Presentation parameters for the first tier of the clear-horizon confirmation.
///
/// The first tier states what survives the reset: only the implementation steps
/// are dropped, while the `<concept>` tree of requirements and invariants stays
/// untouched (c-2). The safe `Cancel` button holds the default focus.
pub fn clear_horizon_params<'a>(state: &'a AppState, plan_id: &str) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        clear_horizon_plan_line(state, plan_id),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.clear_horizon.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.clear_horizon"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: CLEAR_HORIZON_WIDTH,
    }
}

/// Presentation parameters for the second tier of the clear-horizon confirmation.
///
/// A plan whose steps are already `InProgress` or `Done` has committed work in
/// Git. Clearing its horizon does not rewrite that history, so the second tier
/// raises the desynchronization risk explicitly before the reset runs (c-2).
pub fn clear_horizon_final_params<'a>(
    state: &'a AppState,
    plan_id: &str,
) -> ConfirmModalParams<'a> {
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon_final.question"),
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        clear_horizon_plan_line(state, plan_id),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon_final.git_note"),
            engine.fg(palette.status_warning).bold(),
        )),
    ];

    ConfirmModalParams {
        title: active_t("dialog.clear_horizon_final.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_clear_horizon"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.confirm_button,
        destructive: true,
        width: CLEAR_HORIZON_FINAL_WIDTH,
    }
}

pub fn render_confirm_clear_horizon_popup(
    frame: &mut ratatui::Frame,
    plan_id: &str,
    state: &AppState,
) {
    render_modal_confirm(frame, clear_horizon_params(state, plan_id));
}

pub fn render_confirm_clear_horizon_final_popup(
    frame: &mut ratatui::Frame,
    plan_id: &str,
    state: &AppState,
) {
    render_modal_confirm(frame, clear_horizon_final_params(state, plan_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn plan_item(
        id: &str,
        title: &str,
        status: tauqe_protocol::PlanItemStatus,
    ) -> tauqe_protocol::PlanItem {
        tauqe_protocol::PlanItem {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Default::default()
        }
    }

    fn review_issue(id: u32, issue_type: tauqe_protocol::IssueType) -> tauqe_protocol::ReviewIssue {
        tauqe_protocol::ReviewIssue {
            id,
            title: format!("Finding {id}"),
            uuid: None,
            group: None,
            model: None,
            issue_type,
            status: tauqe_protocol::IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
            plan: Vec::new(),
            note: None,
        }
    }

    /// The geometry used for mouse hit-testing must land exactly on the drawn
    /// action buttons, regardless of label length or body line count.
    #[test]
    fn rendered_confirm_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 30);
        let params = ConfirmModalParams {
            title: "Confirm Test".to_string(),
            tone: ModalTone::Dangerous,
            body_lines: vec![Line::raw(""), Line::from("Body"), Line::raw("")],
            confirm_label: "Resolve (Enter)".to_string(),
            cancel_label: "Cancel (Esc)".to_string(),
            confirm_button: crate::app::ConfirmDialogButton::Cancel,
            destructive: true,
            width: 66,
        };
        let body_count = params.body_lines.len();

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_modal_confirm(f, params)).unwrap();

        let geom = crate::components::confirm::confirm_modal_geometry(
            term,
            66,
            body_count,
            "Resolve (Enter)",
            "Cancel (Esc)",
        );
        let buffer = terminal.backend().buffer();

        let confirm_text: String = (geom.confirm_button.x
            ..geom.confirm_button.x + geom.confirm_button.width)
            .map(|x| buffer[(x, geom.confirm_button.y)].symbol())
            .collect();
        assert!(
            confirm_text.contains("Resolve"),
            "confirm row text: {confirm_text:?}"
        );

        let cancel_text: String = (geom.cancel_button.x
            ..geom.cancel_button.x + geom.cancel_button.width)
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
        st.confirm_execute_review = Some(crate::app::ConfirmExecuteReviewState::new(
            None,
            "All Review Sessions".to_string(),
            "All remaining TODO findings across all sessions (2 items)".to_string(),
            None,
            None,
            vec![
                review_issue(1, tauqe_protocol::IssueType::Bug),
                review_issue(2, tauqe_protocol::IssueType::Quality),
            ],
            Vec::new(),
        ));

        let params = execute_review_params(&st).expect("params should be generated");
        assert_eq!(params.title, "Execute All Review Sessions");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(!params.destructive);
        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts
            .iter()
            .any(|t| t.contains("Review: All Review Sessions")));
        assert!(texts.iter().any(|t| t.contains("2 finding(s) to resolve")));
    }

    /// The first tier of plan deletion reports the derived status and the
    /// amount of unfinished work, and keeps the safe `Cancel` default (c-1).
    #[test]
    fn test_delete_plan_params_report_unfinished_work() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-open".to_string(),
            title: "Open Plan".to_string(),
            items: vec![
                plan_item("1", "Done step", tauqe_protocol::PlanItemStatus::Done),
                plan_item("2", "Todo step", tauqe_protocol::PlanItemStatus::Todo),
            ],
            ..Default::default()
        }];

        let params = delete_plan_params(&st, "plan-open");
        assert_eq!(params.title, "Delete Plan");
        assert_eq!(params.confirm_label, "Confirm Delete");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        assert_eq!(params.width, DELETE_PLAN_WIDTH);
        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Open Plan (plan-open)")));
        assert!(texts.iter().any(|t| t.contains("IN_PROGRESS")));
        assert!(texts.iter().any(|t| t.contains("1 step(s)")), "{texts:?}");

        // A finished plan needs no escalation and states it.
        if let Some(plan) = st.plans_view.plans.first_mut() {
            plan.items[1].status = tauqe_protocol::PlanItemStatus::Done;
        }
        let done_params = delete_plan_params(&st, "plan-open");
        let done_texts: Vec<String> = done_params
            .body_lines
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(done_texts.iter().any(|t| t.contains("all steps completed")));
    }

    /// The second tier escalates to the error tone and names the remaining work
    /// before the plan can be deleted (c-1).
    #[test]
    fn test_delete_plan_final_params_are_dangerous() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-wip".to_string(),
            title: "Work In Progress".to_string(),
            items: vec![
                plan_item("1", "Done step", tauqe_protocol::PlanItemStatus::Done),
                plan_item(
                    "2",
                    "In progress step",
                    tauqe_protocol::PlanItemStatus::InProgress,
                ),
            ],
            ..Default::default()
        }];

        let params = delete_plan_final_params(&st, "plan-wip");
        assert_eq!(params.title, "Confirm Deleting Active Plan");
        assert_eq!(params.tone, ModalTone::Dangerous);
        assert_eq!(params.confirm_label, "Permanently Delete");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        assert_eq!(params.width, DELETE_PLAN_FINAL_WIDTH);

        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("Work In Progress")));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("1 of 2 step(s) unfinished")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("NOT undone")));
    }

    /// The two-tier deletion dialogs must render their buttons exactly where the
    /// mouse hit-tester computes them (Conventions §6, §7).
    #[test]
    fn delete_plan_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 40);
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan {
            id: "plan-geom".to_string(),
            title: "Geometry Plan".to_string(),
            items: vec![plan_item(
                "1",
                "Todo step",
                tauqe_protocol::PlanItemStatus::Todo,
            )],
            ..Default::default()
        }];

        let cases = [
            (delete_plan_params(&st, "plan-geom"), DELETE_PLAN_WIDTH),
            (
                delete_plan_final_params(&st, "plan-geom"),
                DELETE_PLAN_FINAL_WIDTH,
            ),
        ];

        for (params, width) in cases {
            let confirm_label = params.confirm_label.clone();
            let cancel_label = params.cancel_label.clone();
            let body_count = params.body_lines.len();
            let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
            terminal.draw(|f| render_modal_confirm(f, params)).unwrap();

            let geom = crate::components::confirm::confirm_modal_geometry(
                term,
                width,
                body_count,
                &confirm_label,
                &cancel_label,
            );
            let buffer = terminal.backend().buffer();
            let read = |rect: Rect| -> String {
                (rect.x..rect.x + rect.width)
                    .map(|x| buffer[(x, rect.y)].symbol())
                    .collect()
            };

            let confirm_row = read(geom.confirm_button);
            assert!(
                confirm_row.contains(confirm_label.as_str()),
                "confirm row for '{confirm_label}': {confirm_row:?}"
            );
            let cancel_row = read(geom.cancel_button);
            assert!(cancel_row.contains("Cancel"), "{cancel_row:?}");
        }
    }

    #[test]
    fn test_clear_horizon_params_are_warning_and_safe() {
        let st = crate::app::test_state();
        let params = clear_horizon_params(&st, "plan-1");

        assert_eq!(params.title, "Clear Implementation Horizon");
        assert_eq!(params.tone, ModalTone::Warning);
        assert_eq!(params.confirm_label, "Clear Horizon");
        assert_eq!(params.cancel_label, "Cancel");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        assert_eq!(params.width, CLEAR_HORIZON_WIDTH);

        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("plan-1")));
        assert!(texts
            .iter()
            .any(|t| t.contains("architectural concept tree will remain intact")));
    }

    /// The second tier escalates to the dangerous tone and names the Git
    /// desynchronization risk before the reset can run.
    #[test]
    fn test_clear_horizon_final_params_are_dangerous() {
        let st = crate::app::test_state();
        let params = clear_horizon_final_params(&st, "plan-2");

        assert_eq!(params.title, "Confirm Clearing Active Horizon");
        assert_eq!(params.tone, ModalTone::Dangerous);
        assert_eq!(params.confirm_label, "Confirm Clear");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        assert_eq!(params.width, CLEAR_HORIZON_FINAL_WIDTH);

        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(texts.iter().any(|t| t.contains("plan-2")));
        assert!(texts
            .iter()
            .any(|t| t.contains("does NOT undo Git commits")));
    }

    /// Rendered buttons of both tiers must land exactly on the rectangles the
    /// mouse hit-tester computes (Conventions §6, §7).
    #[test]
    fn clear_horizon_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 40);
        let st = crate::app::test_state();

        let cases = [
            (
                clear_horizon_params(&st, "plan-3"),
                "Clear Horizon",
                CLEAR_HORIZON_WIDTH,
            ),
            (
                clear_horizon_final_params(&st, "plan-4"),
                "Confirm Clear",
                CLEAR_HORIZON_FINAL_WIDTH,
            ),
        ];

        for (params, confirm_label, width) in cases {
            let body_count = params.body_lines.len();
            let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
            terminal.draw(|f| render_modal_confirm(f, params)).unwrap();

            let geom = crate::components::confirm::confirm_modal_geometry(
                term,
                width,
                body_count,
                confirm_label,
                "Cancel",
            );
            let buffer = terminal.backend().buffer();
            let read = |rect: Rect| -> String {
                (rect.x..rect.x + rect.width)
                    .map(|x| buffer[(x, rect.y)].symbol())
                    .collect()
            };

            let confirm_row = read(geom.confirm_button);
            assert!(
                confirm_row.contains("Clear"),
                "confirm row for '{confirm_label}': {confirm_row:?}"
            );
            let cancel_row = read(geom.cancel_button);
            assert!(
                cancel_row.contains("Cancel"),
                "cancel row for '{confirm_label}': {cancel_row:?}"
            );
        }
    }
    /// The post-analysis confirmation names the audited session and keeps the
    /// safe `Cancel` default, so a costly classification is never launched by a
    /// stray keystroke (c-4).
    #[test]
    fn test_review_analyze_params_are_safe_and_named() {
        let mut st = crate::app::test_state();
        assert!(review_analyze_params(&st).is_none());

        st.confirm_review_analyze = Some(crate::app::ConfirmReviewAnalyzeState {
            review_id: "rev-9".to_string(),
            review_title: "Backend audit".to_string(),
        });

        let params = review_analyze_params(&st).expect("params should be generated");
        assert_eq!(params.title, "Run Post-Analysis");
        assert_eq!(params.tone, ModalTone::Info);
        assert_eq!(params.confirm_label, "Run Analysis");
        assert_eq!(params.cancel_label, "Cancel");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(!params.destructive);
        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(
            texts.iter().any(|t| t.contains("Backend audit")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("rev-9")), "{texts:?}");
    }
}
