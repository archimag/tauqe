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

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tauqe_tui::projection::*;

use crate::app::capture::{CaptureSite, QuitGuardReason};
use crate::app::{AppState, ConfirmDialogButton};
use crate::components::modal::verb_hint;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::header::truncate_to_width;

use super::{render_modal_confirm, ConfirmModalParams, ModalTone};

pub fn chatgpt_params(dialog: &crate::app::ChatGptDialog) -> ConfirmModalParams<'static> {
    let message = if dialog.browser_opened == Some(true) {
        "dialog.chatgpt.wait"
    } else {
        "dialog.chatgpt.question"
    };
    let mut body_lines = vec![
        Line::raw(active_t(message).replace("{}", &dialog.prompt.model)),
        Line::raw(active_t("dialog.chatgpt.copy")),
    ];
    if dialog.browser_opened == Some(false) {
        body_lines.push(Line::raw(dialog.prompt.authorization_url.clone()));
    }
    ConfirmModalParams {
        title: active_t("dialog.chatgpt.title"),
        tone: ModalTone::Info,
        body_lines,
        confirm_label: active_t("dialog.chatgpt.continue"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: dialog.button,
        destructive: false,
        width: 72,
    }
}

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
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: 58,
    }
}

pub fn render_confirm_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_cancel_params(state));
}

/// Presentation parameters for the quit confirmation.
///
/// The card names the work a quit would discard by reading the one derivation
/// the exit guard itself consults — `AppState::quit_guard_reason` — so the
/// decision to raise the card and the activity it reports can never disagree,
/// and a newly declared capturing editor reaches both surfaces at once
/// (Conventions §5.4, #31).
pub fn confirm_quit_params(state: &AppState) -> ConfirmModalParams<'_> {
    let engine = current_engine();
    let palette = engine.palette();
    let activity = match state.quit_guard_reason() {
        Some(QuitGuardReason::ReviewRunning) => active_t("dialog.confirm_quit.activity_review"),
        Some(QuitGuardReason::ModelBusy) => active_t("dialog.confirm_quit.activity_model"),
        Some(QuitGuardReason::SquashApplying) => {
            active_t("dialog.confirm_quit.activity_squash_applying")
        }
        Some(QuitGuardReason::Editor(CaptureSite::ReplPrompt)) => {
            active_t("dialog.confirm_quit.activity_draft")
        }
        Some(QuitGuardReason::Editor(CaptureSite::SquashCommitMessage)) => {
            active_t("dialog.confirm_quit.activity_squash_message")
        }
        Some(QuitGuardReason::Editor(CaptureSite::SquashCustomBaseRef)) => {
            active_t("dialog.confirm_quit.activity_squash_custom_base")
        }
        Some(QuitGuardReason::Editor(CaptureSite::ReviewLaunchPrompt)) => {
            active_t("dialog.confirm_quit.activity_review_launch_prompt")
        }
        Some(QuitGuardReason::Editor(CaptureSite::ReviewLaunchPath)) => {
            active_t("dialog.confirm_quit.activity_review_launch_path")
        }
        Some(QuitGuardReason::Editor(CaptureSite::PlanRefineInstructions)) => {
            active_t("dialog.confirm_quit.activity_plan_refine")
        }
        Some(QuitGuardReason::Editor(CaptureSite::PlanCreateDirectives)) => {
            active_t("dialog.confirm_quit.activity_plan_create")
        }
        Some(QuitGuardReason::Editor(CaptureSite::ReviewExecuteDirectives)) => {
            active_t("dialog.confirm_quit.activity_review_execute")
        }
        Some(QuitGuardReason::Editor(CaptureSite::DiscussionPrompt)) => {
            active_t("dialog.confirm_quit.activity_discussion_prompt")
        }
        Some(QuitGuardReason::Editor(CaptureSite::LocaleLanguage)) => {
            active_t("dialog.confirm_quit.activity_locale")
        }
        Some(QuitGuardReason::Editor(CaptureSite::ContextPathInput)) => {
            active_t("dialog.confirm_quit.activity_context_path")
        }
        Some(QuitGuardReason::Editor(CaptureSite::OnboardingInput)) => {
            active_t("dialog.confirm_quit.activity_onboarding")
        }
        None => active_t("dialog.confirm_quit.activity_default"),
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
        confirm_button: state.modals.confirm_button,
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
        confirm_button: state.modals.confirm_button,
        destructive: false,
        width: 58,
    }
}

pub fn render_confirm_detach_focus_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, confirm_detach_focus_params(state));
}
pub fn render_confirm_execute_review_popup(frame: &mut ratatui::Frame, state: &AppState) {
    super::execute_review::render_execute_review_dialog(frame, state);
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
    let target = state.modals.confirm_execute_pipeline.as_ref()?;
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
        confirm_button: state.modals.confirm_button,
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
/// The metric is the client leaf accounting of `PlanImplementationProjection::count_stats`, which
/// already excludes cancelled leaves from the total, so a plan whose leaves are
/// all completed or cancelled needs no escalation (c-1).
pub(crate) fn plan_unfinished_steps(plan: &tauqe_protocol::Plan) -> usize {
    let (total, completed) = plan.implementation.count_stats();
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

/// Widest a model- or user-authored identifier may grow inside a confirmation
/// body before it is truncated, matching the queue-row budgets of this file.
///
/// Truncation keeps a card compact; the wrapped-height budget of
/// `confirm_modal_geometry` remains the correctness guarantee that no row clips,
/// whatever the string length.
const CONFIRM_FIELD_WIDTH: usize = 36;

/// Plan field shared by both plan-deletion confirmations and both horizon
/// confirmations: `Plan: <title> (<id>)`.
fn delete_plan_line(state: &AppState, plan_id: &str) -> Line<'static> {
    let engine = current_engine();
    let palette = engine.palette();
    Line::from(vec![
        Span::raw(active_t("dialog.delete_plan.plan_label")),
        Span::styled(
            format!(
                "{} ({})",
                truncate_to_width(&plan_display_title(state, plan_id), CONFIRM_FIELD_WIDTH),
                plan_id
            ),
            engine.fg(palette.status_info).bold(),
        ),
    ])
}

/// Presentation parameters for the first tier of the plan-deletion confirmation.
///
/// The dialog states the derived plan status together with how much unfinished
/// work the deletion would discard, so the cost is visible before confirming.
/// The safe `Cancel` button holds the default focus (Conventions §5.4, c-1).
///
/// It describes the plan the pending first-tier deletion names, so a surface
/// that asks for the card without a pending decision receives `None` instead of
/// a dialog that names nothing (c-1).
pub fn delete_plan_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let plan_id = state.modals.confirm_delete_plan.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();
    let plan = state.plans_view.plans.iter().find(|p| p.id == plan_id);
    let status = plan.map(|p| p.implementation.status()).unwrap_or_default();
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

    Some(ConfirmModalParams {
        title: active_t("dialog.delete_plan.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: DELETE_PLAN_WIDTH,
    })
}

/// Presentation parameters for the second tier of the plan-deletion
/// confirmation.
///
/// A plan whose leaves are still pending, in progress, or blocked carries task
/// context that no Git commit restores, so the second tier names the remaining
/// work explicitly and warns that the recorded progress is lost for good (c-1).
pub fn delete_plan_final_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let plan_id = state.modals.confirm_delete_plan_final.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();
    let plan = state.plans_view.plans.iter().find(|p| p.id == plan_id);
    let (total, completed) = plan
        .map(|p| p.implementation.count_stats())
        .unwrap_or((0, 0));
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

    Some(ConfirmModalParams {
        title: active_t("dialog.delete_plan_final.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.permanently_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: DELETE_PLAN_FINAL_WIDTH,
    })
}

pub fn render_confirm_delete_plan_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = delete_plan_params(state) {
        render_modal_confirm(frame, params);
    }
}

pub fn render_confirm_delete_plan_final_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = delete_plan_final_params(state) {
        render_modal_confirm(frame, params);
    }
}

/// Presentation parameters for the review-deletion confirmation.
pub fn delete_review_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let session_id = state.modals.confirm_delete_review.as_deref()?;
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
                format!(
                    "{} ({})",
                    truncate_to_width(&review_title, CONFIRM_FIELD_WIDTH),
                    session_id
                ),
                engine.fg(palette.status_info).bold(),
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.delete_review.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.delete_review.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_delete"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: 58,
    })
}

pub fn render_confirm_delete_review_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = delete_review_params(state) {
        render_modal_confirm(frame, params);
    }
}
/// Width of the post-analysis confirmation, as a percentage of the terminal.
const REVIEW_ANALYZE_DIALOG_WIDTH: u16 = 68;
/// Height of the post-analysis confirmation, as a percentage of the terminal.
///
/// The dialog carries the Architect ladder instead of a directives editor, so it
/// needs noticeably less vertical room than a picker that stacks a model list
/// above a text field (c-dialog-parity).
const REVIEW_ANALYZE_DIALOG_HEIGHT: u16 = 52;
/// Header rows: the heading, the audited session, and the classification note.
const REVIEW_ANALYZE_HEADER_HEIGHT: u16 = 4;

/// Full drawn confirm-button label of the post-analysis dialog, resolved from
/// the runtime locale (c-1).
fn review_analyze_confirm_label() -> String {
    format!(" [ {} ] ", active_t("dialog.button.run_analysis"))
}

/// Full drawn cancel-button label of the post-analysis dialog, resolved from the
/// runtime locale (c-1).
fn review_analyze_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Rectangles of the post-analysis confirmation, shared by the renderer and the
/// mouse hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReviewAnalyzeDialogGeometry {
    /// Whole dialog frame.
    pub(crate) area: Rect,
    /// Inner rows of the Architect model list (one row per entry).
    pub(crate) model_rows: Rect,
    /// Confirm button.
    pub(crate) confirm_button: Rect,
    /// Cancel button.
    pub(crate) cancel_button: Rect,
}

/// Rows of the Architect ladder, merged with the band the shared picker layout
/// reserves for an instructions editor.
///
/// The post-analysis confirms a round rather than describing one, so it carries
/// no directives editor: the band below the ladder belongs to the list instead
/// of leaving an empty zone under the last rung, exactly as the plan execution
/// dialog behaves (c-dialog-parity, Conventions §6.7).
fn review_analyze_list_area(chunks: &super::PickerDialogChunks) -> Rect {
    let bottom = chunks.editor.y.saturating_add(chunks.editor.height);
    Rect::new(
        chunks.list.x,
        chunks.list.y,
        chunks.list.width,
        bottom.saturating_sub(chunks.list.y),
    )
}

/// Computes the dialog frame, the Architect-row area, and the action buttons
/// from the shared picker layout the renderer uses, so the drawn buttons are
/// always the clickable ones (Conventions §6, §7).
pub(crate) fn review_analyze_dialog_geometry(
    term: Rect,
    entry_count: usize,
) -> ReviewAnalyzeDialogGeometry {
    let area = crate::ui::centered_rect(
        REVIEW_ANALYZE_DIALOG_WIDTH,
        REVIEW_ANALYZE_DIALOG_HEIGHT,
        term,
    );
    let chunks = super::picker_dialog_chunks(area, entry_count, REVIEW_ANALYZE_HEADER_HEIGHT, 0);

    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) = super::picker_button_rects(
        chunks.buttons,
        &review_analyze_confirm_label(),
        &review_analyze_cancel_label(),
    );

    ReviewAnalyzeDialogGeometry {
        area,
        model_rows: super::inset(review_analyze_list_area(&chunks)),
        confirm_button,
        cancel_button,
    }
}

/// Renders the Architect post-analysis confirmation.
///
/// The classification is a costly LLM layer over immutable findings, so the
/// dialog names the audited session and the Architect rung the round runs on
/// before any token is spent. It stays a confirmation: the ladder changes the
/// configuration of the round, never its contract, and the safe `Cancel` button
/// holds the default focus (c-dialog-parity, Conventions §5.4, §6).
pub fn render_confirm_review_analyze_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = state.modals.confirm_review_analyze.as_ref() else {
        return;
    };

    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let entry_count = target.selector.len();
    let area = crate::ui::centered_rect(
        REVIEW_ANALYZE_DIALOG_WIDTH,
        REVIEW_ANALYZE_DIALOG_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", active_t("dialog.review_analyze.title")))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.button_active_bg))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let chunks = super::picker_dialog_chunks(area, entry_count, REVIEW_ANALYZE_HEADER_HEIGHT, 0);
    let header_area = chunks.header;
    // The picker layout reserves an instructions-editor band below the ladder.
    // The post-analysis carries no editor, so the list claims that band and the
    // card keeps no empty zone under the last rung (c-dialog-parity).
    let list_area = review_analyze_list_area(&chunks);
    let buttons_area = chunks.buttons;
    let footer_area = chunks.footer;

    // Header: which session the classification rewrites and what the layer means.
    let header_lines = vec![
        Line::from(Span::styled(
            truncate_to_width(&active_t("dialog.review_analyze.heading"), 62),
            engine.fg(palette.status_info).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(active_t("dialog.review_analyze.review_label"), dim),
            Span::styled(
                target.review_title.clone(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(format!(" [{}]", target.review_id), dim),
        ]),
        Line::from(Span::styled(
            truncate_to_width(&active_t("dialog.review_analyze.note"), 62),
            dim,
        )),
    ];
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Architect ladder. The shared `ModelSelector` owns the rows, numbering,
    // scrolling, and empty-list safety; the block adds framing and the focus
    // color.
    let list_title = if entry_count > 1 {
        active_t("dialog.review_analyze.model_list_counter")
            .replacen("{}", &(target.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.review_analyze.model_list_title")
    };
    let list_border = if target.focus.is_model_list() {
        engine.fg(palette.status_info).bold()
    } else {
        engine.fg(palette.border)
    };
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(list_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    let list_inner = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);
    if entry_count == 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                active_t("dialog.review_analyze.no_models"),
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = target
            .selector
            .render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Action bar with focus-aware styling.
    let in_actions = target.focus.is_actions();
    let confirm_selected = target.confirm_button == ConfirmDialogButton::Confirm;
    let (confirm_style, cancel_style) = if in_actions {
        if confirm_selected {
            (
                engine
                    .bg(palette.status_success)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
            )
        } else {
            (
                engine
                    .bg(palette.button_bg)
                    .fg(engine.color(palette.button_fg)),
                engine
                    .bg(palette.button_active_bg)
                    .fg(engine.color(palette.text_inverse))
                    .bold(),
            )
        }
    } else {
        (
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
            engine
                .bg(palette.button_bg)
                .fg(engine.color(palette.button_fg)),
        )
    };
    let actions = Line::from(vec![
        Span::styled(review_analyze_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(review_analyze_cancel_label(), cancel_style),
    ]);
    frame.render_widget(
        Paragraph::new(actions).alignment(Alignment::Center),
        buttons_area,
    );

    // The hint is docked to the card's bottom edge as its own strip filled with
    // `palette.overlay`, so it reads as persistent chrome instead of a second
    // row of buttons (Conventions §6.4).
    let primary = state.tui_config.input.primary_modifier;
    let nav_hint = if in_actions {
        active_t("dialog.review_analyze.hint_actions")
    } else {
        active_t("dialog.review_analyze.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    crate::components::modal::render_dialog_footer(frame, footer_area, &verb_hint(nav_hint));
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
        confirm_button: state.modals.confirm_button,
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
        confirm_button: state.modals.confirm_button,
        destructive: false,
        width: 58,
    }
}

pub fn render_confirm_clear_user_popup(frame: &mut ratatui::Frame, state: &AppState) {
    render_modal_confirm(frame, clear_user_params(state));
}

/// Presentation parameters for the single-file context removal confirmation.
///
/// A curated audit context is destructive to lose, so removing one file travels
/// through the same shared card as every other context mutation: the dialog
/// names the exact path, the safe `Cancel` button holds the default focus, and
/// the pending path carried by the modal state is what the confirmation turns
/// into a `context/remove` request (Conventions §4.3, §5.4).
pub fn remove_context_file_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let path = state.modals.confirm_remove_context_file.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.remove_context_file.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                active_t("dialog.remove_context_file.file_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(
                truncate_to_width(path, CONFIRM_FIELD_WIDTH),
                engine.fg(palette.status_info).bold(),
            ),
        ]),
        Line::from(Span::styled(
            active_t("dialog.remove_context_file.note"),
            engine.fg(palette.text_muted),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.remove_context_file.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.remove"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: 60,
    })
}

pub fn render_confirm_remove_context_file_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = remove_context_file_params(state) {
        render_modal_confirm(frame, params);
    }
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
        confirm_button: state.modals.confirm_button,
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

/// Presentation parameters for the first tier of the clear-horizon confirmation.
///
/// The first tier states what survives the reset: only the implementation steps
/// are dropped, while the `<concept>` tree of requirements and invariants stays
/// untouched (c-2). The safe `Cancel` button holds the default focus.
pub fn clear_horizon_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let plan_id = state.modals.confirm_clear_horizon.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        delete_plan_line(state, plan_id),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon.note"),
            engine.fg(palette.text_secondary),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.clear_horizon.title"),
        tone: ModalTone::Warning,
        body_lines,
        confirm_label: active_t("dialog.button.clear_horizon"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: CLEAR_HORIZON_WIDTH,
    })
}

/// Presentation parameters for the second tier of the clear-horizon confirmation.
///
/// A plan whose steps are already `InProgress` or `Done` has committed work in
/// Git. Clearing its horizon does not rewrite that history, so the second tier
/// raises the desynchronization risk explicitly before the reset runs (c-2).
pub fn clear_horizon_final_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let plan_id = state.modals.confirm_clear_horizon_final.as_deref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let body_lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon_final.question"),
            engine.fg(palette.status_error).bold(),
        )),
        Line::raw(""),
        delete_plan_line(state, plan_id),
        Line::from(Span::styled(
            active_t("dialog.clear_horizon_final.git_note"),
            engine.fg(palette.status_warning).bold(),
        )),
    ];

    Some(ConfirmModalParams {
        title: active_t("dialog.clear_horizon_final.title"),
        tone: ModalTone::Dangerous,
        body_lines,
        confirm_label: active_t("dialog.button.confirm_clear_horizon"),
        cancel_label: active_t("modal.cancel"),
        confirm_button: state.modals.confirm_button,
        destructive: true,
        width: CLEAR_HORIZON_FINAL_WIDTH,
    })
}

pub fn render_confirm_clear_horizon_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = clear_horizon_params(state) {
        render_modal_confirm(frame, params);
    }
}

pub fn render_confirm_clear_horizon_final_popup(frame: &mut ratatui::Frame, state: &AppState) {
    if let Some(params) = clear_horizon_final_params(state) {
        render_modal_confirm(frame, params);
    }
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
            commit: None,
            group: None,
            model: None,
            issue_type,
            status: tauqe_protocol::IssueStatus::Todo,
            description: String::new(),
            recommendations: Vec::new(),
            files: Vec::new(),
            readonly_files: Vec::new(),
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
        // The geometry is derived from the very body the renderer draws, so the
        // budget and the drawn rows are one artifact (Conventions §1.3, §6.7).
        let geom = crate::components::confirm::confirm_modal_geometry(
            term,
            66,
            &params.body_lines,
            "Resolve (Enter)",
            "Cancel (Esc)",
        );

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_modal_confirm(f, params)).unwrap();
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

    /// The first tier of plan deletion reports the derived status and the
    /// amount of unfinished work, and keeps the safe `Cancel` default (c-1).
    #[test]
    fn test_delete_plan_params_report_unfinished_work() {
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-open".to_string(),
            "Open Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![
                plan_item("1", "Done step", tauqe_protocol::PlanItemStatus::Done),
                plan_item("2", "Todo step", tauqe_protocol::PlanItemStatus::Todo),
            ],
        )];

        st.modals.confirm_delete_plan = Some("plan-open".to_string());
        let params = delete_plan_params(&st).expect("the pending deletion describes its card");
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
            plan.implementation.items[1].status = tauqe_protocol::PlanItemStatus::Done;
        }
        let done_params = delete_plan_params(&st).expect("the pending deletion describes its card");
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
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-wip".to_string(),
            "Work In Progress".to_string(),
            None,
            0,
            0,
            vec![],
            vec![
                plan_item("1", "Done step", tauqe_protocol::PlanItemStatus::Done),
                plan_item(
                    "2",
                    "In progress step",
                    tauqe_protocol::PlanItemStatus::InProgress,
                ),
            ],
        )];

        st.modals.confirm_delete_plan_final = Some("plan-wip".to_string());
        let params =
            delete_plan_final_params(&st).expect("the pending deletion describes its card");
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
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-geom".to_string(),
            "Geometry Plan".to_string(),
            None,
            0,
            0,
            vec![],
            vec![plan_item(
                "1",
                "Todo step",
                tauqe_protocol::PlanItemStatus::Todo,
            )],
        )];

        st.modals.confirm_delete_plan = Some("plan-geom".to_string());
        st.modals.confirm_delete_plan_final = Some("plan-geom".to_string());

        let cases = [
            (
                delete_plan_params(&st).expect("the pending deletion describes its card"),
                DELETE_PLAN_WIDTH,
            ),
            (
                delete_plan_final_params(&st).expect("the pending deletion describes its card"),
                DELETE_PLAN_FINAL_WIDTH,
            ),
        ];

        for (params, width) in cases {
            let confirm_label = params.confirm_label.clone();
            let cancel_label = params.cancel_label.clone();
            // The geometry is derived from the body and the labels the renderer
            // receives, so the drawn buttons and the clickable buttons coincide.
            let geom = crate::components::confirm::confirm_modal_geometry(
                term,
                width,
                &params.body_lines,
                &confirm_label,
                &cancel_label,
            );
            let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
            terminal.draw(|f| render_modal_confirm(f, params)).unwrap();
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
        let mut st = crate::app::test_state();
        st.modals.confirm_clear_horizon = Some("plan-1".to_string());
        let params = clear_horizon_params(&st).expect("the pending reset describes its card");

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
        let mut st = crate::app::test_state();
        st.modals.confirm_clear_horizon_final = Some("plan-2".to_string());
        let params = clear_horizon_final_params(&st).expect("the pending reset describes its card");

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
        let mut st = crate::app::test_state();
        st.modals.confirm_clear_horizon = Some("plan-3".to_string());
        st.modals.confirm_clear_horizon_final = Some("plan-4".to_string());

        let cases = [
            (
                clear_horizon_params(&st).expect("the pending reset describes its card"),
                "Clear Horizon",
                CLEAR_HORIZON_WIDTH,
            ),
            (
                clear_horizon_final_params(&st).expect("the pending reset describes its card"),
                "Confirm Clear",
                CLEAR_HORIZON_FINAL_WIDTH,
            ),
        ];

        for (params, confirm_label, width) in cases {
            let geom = crate::components::confirm::confirm_modal_geometry(
                term,
                width,
                &params.body_lines,
                confirm_label,
                "Cancel",
            );
            let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
            terminal.draw(|f| render_modal_confirm(f, params)).unwrap();
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
    /// The post-analysis dialog names the audited session, lists every configured
    /// Architect entry, and keeps the safe `Cancel` default focus, so a costly
    /// classification is never launched by a stray keystroke (c-dialog-parity).
    #[test]
    fn test_review_analyze_dialog_lists_the_architect_ladder() {
        let mut st = crate::app::test_state();
        st.modals.confirm_review_analyze = Some(crate::app::ConfirmReviewAnalyzeState::new(
            "rev-9".to_string(),
            "Backend audit".to_string(),
            vec![
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("architect-a"),
                    None,
                ),
                tauqe_protocol::ModelEntry::new(
                    tauqe_protocol::ModelRef::openrouter("architect-b"),
                    Some(tauqe_protocol::ReasoningEffort::High),
                ),
            ],
        ));
        let target = st
            .modals
            .confirm_review_analyze
            .as_ref()
            .expect("dialog open");
        assert_eq!(target.selector.len(), 2);
        assert_eq!(
            target.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| render_confirm_review_analyze_popup(f, &st))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for row in 0..40u16 {
            for column in 0..120u16 {
                screen.push_str(buffer[(column, row)].symbol());
            }
            screen.push('\n');
        }
        assert!(screen.contains("Backend audit"), "{screen}");
        assert!(screen.contains("rev-9"), "{screen}");
        assert!(screen.contains("architect-a"), "{screen}");
        assert!(screen.contains("architect-b [high]"), "{screen}");
    }

    /// The geometry used for mouse hit-testing must land exactly on the drawn
    /// action buttons, whatever the localized labels are (Conventions §6, §7).
    #[test]
    fn rendered_review_analyze_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 40);
        let mut st = crate::app::test_state();
        st.modals.confirm_review_analyze = Some(crate::app::ConfirmReviewAnalyzeState::new(
            "rev-1".to_string(),
            "Session rev-1".to_string(),
            vec![tauqe_protocol::ModelEntry::new(
                tauqe_protocol::ModelRef::openrouter("architect-a"),
                None,
            )],
        ));
        let entry_count = st
            .modals
            .confirm_review_analyze
            .as_ref()
            .expect("dialog open")
            .selector
            .len();

        let geom = review_analyze_dialog_geometry(term, entry_count);
        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_confirm_review_analyze_popup(f, &st))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };
        assert!(
            read(geom.confirm_button).contains("Run Analysis"),
            "confirm row: {:?}",
            read(geom.confirm_button)
        );
        assert!(
            read(geom.cancel_button).contains("Cancel"),
            "cancel row: {:?}",
            read(geom.cancel_button)
        );
    }
    /// The single-file removal confirmation names the pending path, warns with
    /// the shared tone, and keeps the safe `Cancel` default (c-4).
    #[test]
    fn test_remove_context_file_params_name_the_pending_path() {
        let mut st = crate::app::test_state();
        assert!(remove_context_file_params(&st).is_none());

        st.modals.confirm_remove_context_file = Some("crates/tui/src/app.rs".to_string());

        let params = remove_context_file_params(&st).expect("params should be generated");
        assert_eq!(params.title, "Remove File from Context");
        assert_eq!(params.tone, ModalTone::Warning);
        assert_eq!(params.confirm_label, "Remove");
        assert_eq!(params.cancel_label, "Cancel");
        assert_eq!(
            params.confirm_button,
            crate::app::ConfirmDialogButton::Cancel
        );
        assert!(params.destructive);
        let texts: Vec<String> = params.body_lines.iter().map(|l| l.to_string()).collect();
        assert!(
            texts.iter().any(|t| t.contains("crates/tui/src/app.rs")),
            "{texts:?}"
        );
    }
    /// A model- or user-authored string can push a body line past the card's
    /// inner width, so the body occupies more visual rows than it has logical
    /// lines. The wrapped budget must keep every one of those rows inside the
    /// card — most importantly the trailing consent line (§5.4, §6.7).
    #[test]
    fn wrapped_body_keeps_the_trailing_note_inside_the_card() {
        // A terminal height that divides 100 exactly keeps the percentage split
        // lossless, so the assertions measure the budget rather than the
        // rounding of `centered_rect`.
        let term = Rect::new(0, 0, 60, 50);
        let mut st = crate::app::test_state();
        st.plans_view.plans = vec![tauqe_protocol::Plan::from_parts(
            "plan-long-title".to_string(),
            "W".repeat(400),
            None,
            0,
            0,
            vec![],
            vec![plan_item(
                "1",
                "Todo step",
                tauqe_protocol::PlanItemStatus::Todo,
            )],
        )];

        st.modals.confirm_delete_plan = Some("plan-long-title".to_string());
        let params = delete_plan_params(&st).expect("the pending deletion describes its card");
        let note = params
            .body_lines
            .last()
            .map(|line| line.to_string())
            .expect("the card carries a trailing note");
        let geom = crate::components::confirm::confirm_modal_geometry(
            term,
            params.width,
            &params.body_lines,
            &params.confirm_label,
            &params.cancel_label,
        );

        let body = crate::components::modal::modal_geometry(geom.area).body;
        let rows: usize = params
            .body_lines
            .iter()
            .map(|line| crate::ui::wrap_line(line.clone(), usize::from(body.width)).len())
            .sum();
        assert!(
            usize::from(body.height) >= rows,
            "the card reserves {} rows for {rows} wrapped rows",
            body.height
        );

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal.draw(|f| render_modal_confirm(f, params)).unwrap();

        let buffer = terminal.backend().buffer();
        let card: String = (geom.area.y..geom.area.bottom())
            .map(|row| {
                (geom.area.x..geom.area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tail: String = note
            .chars()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            card.contains(&tail),
            "the trailing note must stay inside the card, tail {tail:?}:\n{card}"
        );
    }
    /// The quit card names the input the exit guard protects: both read one
    /// derivation, so every guarded reason renders its own activity line and a
    /// typed draft never falls through to the generic default (#31,
    /// Conventions §5.4).
    #[test]
    fn test_confirm_quit_activity_names_every_guarded_input() {
        use crate::app::capture::{CaptureSite, QuitGuardReason};
        use crate::app::{
            ConfirmExecuteReviewState, LocaleDialogState, PromptModalState, PromptModalTarget,
            ReviewDialogState, SquashDialogState,
        };
        use crate::views::plans::{PlanCreateDialogState, PlanRefineDialogState};
        use crate::views::repl::TurnStatus;

        fn editor_with(text: &str) -> crate::editor::InputEditor {
            let mut editor = crate::editor::InputEditor::default();
            editor.insert_str(text);
            editor
        }

        fn card_lines(st: &AppState) -> Vec<String> {
            let params = confirm_quit_params(st);
            let body = params.body_lines;
            body.iter().map(|line| line.to_string()).collect()
        }

        fn assert_activity(st: &AppState, reason: QuitGuardReason, key: &str) {
            assert_eq!(
                st.quit_guard_reason(),
                Some(reason),
                "the scenario must trip the guarded reason"
            );
            let expected = active_t(key);
            let default = active_t("dialog.confirm_quit.activity_default");
            let lines = card_lines(st);
            let named = lines
                .iter()
                .filter(|line| line.as_str() == expected.as_str())
                .count();
            let fallback = lines
                .iter()
                .filter(|line| line.as_str() == default.as_str())
                .count();
            assert_eq!(named, 1, "the card names {key} once: {lines:?}");
            assert_eq!(fallback, 0, "the default line was rendered: {lines:?}");
        }

        // A running review.
        let mut st = crate::app::test_state();
        st.review.running = true;
        let key = "dialog.confirm_quit.activity_review";
        assert_activity(&st, QuitGuardReason::ReviewRunning, key);

        // A bounded turn in flight.
        let mut st = crate::app::test_state();
        st.model.status = TurnStatus::Thinking;
        let key = "dialog.confirm_quit.activity_model";
        assert_activity(&st, QuitGuardReason::ModelBusy, key);

        // The REPL prompt draft.
        let mut st = crate::app::test_state();
        st.input_editor.insert_str("draft prompt");
        let key = "dialog.confirm_quit.activity_draft";
        assert_activity(&st, QuitGuardReason::Editor(CaptureSite::ReplPrompt), key);

        // A squash commit that is being applied.
        let mut st = crate::app::test_state();
        st.modals.squash_dialog = Some(SquashDialogState {
            applying: true,
            ..Default::default()
        });
        let key = "dialog.confirm_quit.activity_squash_applying";
        assert_activity(&st, QuitGuardReason::SquashApplying, key);

        // The squash commit message.
        let mut st = crate::app::test_state();
        st.modals.squash_dialog = Some(SquashDialogState {
            message_editor: editor_with("feat(tui): name the input"),
            ..Default::default()
        });
        let key = "dialog.confirm_quit.activity_squash_message";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::SquashCommitMessage),
            key,
        );

        // The custom squash base reference.
        let mut st = crate::app::test_state();
        st.modals.squash_dialog = Some(SquashDialogState {
            custom_editor: editor_with("origin/main"),
            custom_input_active: true,
            ..Default::default()
        });
        let key = "dialog.confirm_quit.activity_squash_custom_base";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::SquashCustomBaseRef),
            key,
        );

        // The review-launch instructions prompt.
        let mut st = crate::app::test_state();
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        dialog.prompt_editor.insert_str("focus on error handling");
        st.modals.review_dialog = Some(dialog);
        let key = "dialog.confirm_quit.activity_review_launch_prompt";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::ReviewLaunchPrompt),
            key,
        );

        // The review-launch target paths.
        let mut st = crate::app::test_state();
        let mut dialog = ReviewDialogState::new(0, 0, Vec::new(), Vec::new());
        dialog.path_inputs[0].insert_str("crates/tui/");
        st.modals.review_dialog = Some(dialog);
        let key = "dialog.confirm_quit.activity_review_launch_path";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::ReviewLaunchPath),
            key,
        );

        // The plan-refinement instructions.
        let mut st = crate::app::test_state();
        let mut dialog =
            PlanRefineDialogState::new("plan-1".to_string(), "Plan 1".to_string(), Vec::new());
        dialog
            .instructions_editor
            .insert_str("split the second step");
        st.modals.refine_dialog = Some(dialog);
        let key = "dialog.confirm_quit.activity_plan_refine";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::PlanRefineInstructions),
            key,
        );

        // The plan-creation directives.
        let mut st = crate::app::test_state();
        let mut dialog = PlanCreateDialogState::new(Vec::new());
        dialog.directives_editor.insert_str("add a retry budget");
        st.modals.create_dialog = Some(dialog);
        let key = "dialog.confirm_quit.activity_plan_create";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::PlanCreateDirectives),
            key,
        );

        // The single-target review-execution directives.
        let mut st = crate::app::test_state();
        let mut dialog = ConfirmExecuteReviewState::new(
            Some("rev-1".to_string()),
            "Session rev-1".to_string(),
            "Finding 1".to_string(),
            Some(1),
            vec![review_issue(1, tauqe_protocol::IssueType::Bug)],
            Vec::new(),
        );
        dialog.instructions_editor.insert_str("guard the header");
        st.modals.confirm_execute_review = Some(dialog);
        let key = "dialog.confirm_quit.activity_review_execute";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::ReviewExecuteDirectives),
            key,
        );

        // The discussion message of the prompt modal.
        let mut st = crate::app::test_state();
        let mut dialog = PromptModalState::new(PromptModalTarget::Plan {
            plan_id: "plan-1".to_string(),
            plan_title: "Plan 1".to_string(),
            focused_items: Vec::new(),
        });
        dialog.prompt_editor.insert_str("what about the budget?");
        st.modals.prompt_modal = Some(dialog);
        let key = "dialog.confirm_quit.activity_discussion_prompt";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::DiscussionPrompt),
            key,
        );

        // The interface language field.
        let mut st = crate::app::test_state();
        let mut dialog = LocaleDialogState::default();
        dialog.language_editor.insert_str("French");
        st.modals.locale_dialog = Some(dialog);
        let key = "dialog.confirm_quit.activity_locale";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::LocaleLanguage),
            key,
        );

        // The inline Context path input.
        let mut st = crate::app::test_state();
        st.context_view.adding_file = true;
        st.context_view.add_input.insert_str("crates/");
        let key = "dialog.confirm_quit.activity_context_path";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::ContextPathInput),
            key,
        );

        // The onboarding setup field.
        let mut st = crate::app::test_state();
        st.onboarding.input_active = true;
        st.onboarding.input_buffer.insert_str("sk-or-secret");
        let key = "dialog.confirm_quit.activity_onboarding";
        assert_activity(
            &st,
            QuitGuardReason::Editor(CaptureSite::OnboardingInput),
            key,
        );

        // With nothing at stake the card keeps its generic line.
        let idle = crate::app::test_state();
        assert_eq!(idle.quit_guard_reason(), None);
        let default = active_t("dialog.confirm_quit.activity_default");
        let lines = card_lines(&idle);
        let shown = lines
            .iter()
            .filter(|line| line.as_str() == default.as_str())
            .count();
        assert_eq!(shown, 1, "the idle card keeps the default line");
    }
}
