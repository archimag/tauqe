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

/// Presentation parameters for the plan-step execution confirmation.
pub fn execute_scope_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_execute_scope.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let is_single = target.steps.len() == 1;
    // A scope without any scheduled leaf is a concept-only plan whose horizon
    // the batch projects just-in-time, so the dialog states that explicitly
    // instead of presenting an empty queue as a finished one (c-5).
    let is_jit = target.steps.is_empty();
    let title = if is_single {
        active_t("dialog.execute_scope.title_single")
    } else if is_jit {
        active_t("dialog.execute_scope.title_jit")
    } else {
        active_t("dialog.execute_scope.title_scope")
    };

    let heading = if is_single {
        active_t("dialog.execute_scope.heading_single")
    } else if is_jit {
        active_t("dialog.execute_scope.heading_jit")
    } else {
        active_t("dialog.execute_scope.heading_scope")
    };

    let mut body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(heading, engine.fg(palette.status_info).bold())),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_scope.plan_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(&target.plan_title, engine.fg(palette.text_primary).bold()),
            Span::styled(
                format!(" [{}]", target.plan_id),
                engine.fg(palette.text_muted),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_scope.scope_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                &target.scope_title,
                engine.fg(palette.status_warning).bold(),
            ),
            Span::styled(
                active_t("dialog.execute_scope.scope_count").replacen(
                    "{}",
                    &target.steps.len().to_string(),
                    1,
                ),
                engine.fg(palette.text_primary),
            ),
        ]),
    ];

    if is_single {
        let step = &target.steps[0];
        if let Some(details) = &step.details {
            let trimmed = details.trim();
            if !trimmed.is_empty() {
                let first_line = trimmed.lines().next().unwrap_or("");
                body_lines.push(Line::from(vec![
                    Span::styled(
                        active_t("dialog.execute_scope.details_label"),
                        engine.fg(palette.text_muted),
                    ),
                    Span::styled(
                        truncate_to_width(first_line, 45),
                        engine.fg(palette.text_secondary),
                    ),
                ]));
            }
        }
    } else {
        body_lines.push(Line::raw(""));
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_scope.queue_header"),
            engine.fg(palette.text_muted),
        )));
        for (i, step) in target.steps.iter().take(4).enumerate() {
            body_lines.push(Line::from(vec![
                Span::styled(format!("   {}. ", i + 1), engine.fg(palette.status_info)),
                Span::styled(
                    format!("#{} {}", step.id, truncate_to_width(&step.title, 40)),
                    engine.fg(palette.text_primary),
                ),
            ]));
        }
        if target.steps.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                active_t("dialog.execute_scope.queue_more").replacen(
                    "{}",
                    &(target.steps.len() - 4).to_string(),
                    1,
                ),
                engine.fg(palette.text_muted).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.execute_scope.note"),
        engine.fg(palette.text_muted),
    )));
    if is_jit {
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_scope.jit_note"),
            engine.fg(palette.text_secondary),
        )));
    }
    if !is_single {
        body_lines.push(Line::from(Span::styled(
            active_t("dialog.execute_scope.fail_fast"),
            engine.fg(palette.text_muted),
        )));
    }

    let confirm_label = if is_single {
        active_t("dialog.button.execute")
    } else {
        active_t("dialog.button.execute_scope")
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

pub fn render_confirm_execute_scope_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = execute_scope_params(state) {
        render_modal_confirm(frame, params);
    }
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
            active_t("dialog.delete_plan.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw(active_t("dialog.delete_plan.plan_label")),
            Span::styled(
                format!("{} ({})", plan_title, plan_id),
                engine.fg(palette.status_info).bold(),
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
        width: 58,
    }
}

pub fn render_confirm_delete_plan_popup(
    frame: &mut ratatui::Frame,
    plan_id: &str,
    state: &AppState,
) {
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

/// Title of the plan the confirmation targets, resolved from the loaded plans.
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

/// Plan field of both clear-horizon confirmations: `Plan: <title> (<id>)`.
fn clear_horizon_plan_line<'a>(state: &AppState, plan_id: &str) -> Line<'a> {
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
                tauqe_protocol::ReviewIssue {
                    id: 1,
                    title: "First".to_string(),
                    model: None,
                    issue_type: tauqe_protocol::IssueType::Bug,
                    status: tauqe_protocol::IssueStatus::Todo,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                },
                tauqe_protocol::ReviewIssue {
                    id: 2,
                    title: "Second".to_string(),
                    model: None,
                    issue_type: tauqe_protocol::IssueType::Quality,
                    status: tauqe_protocol::IssueStatus::Todo,
                    description: String::new(),
                    recommendations: Vec::new(),
                    files: Vec::new(),
                    readonly_files: Vec::new(),
                    plan: Vec::new(),
                    note: None,
                },
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

    /// The first tier keeps the safe defaults: warning tone, `Cancel` focused,
    /// and the concept-preservation note in the body.
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
}
