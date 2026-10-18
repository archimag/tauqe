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

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{AppState, BatchCancelButton};
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
    } else if state.squash_dialog.as_ref().is_some_and(|d| !d.message_editor.is_empty()) {
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
            Span::styled(&target.scope_title, engine.fg(palette.status_warning).bold()),
            Span::styled(
                active_t("dialog.execute_review.scope_count")
                    .replacen("{}", &target.items.len().to_string(), 1),
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
                Span::styled(format!("[{}] #{} {}", item.issue_type, item.id, truncate_to_width(&item.title, 36)), engine.fg(palette.text_primary)),
            ]));
        }
        if target.items.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                active_t("dialog.execute_review.queue_more")
                    .replacen("{}", &(target.items.len() - 4).to_string(), 1),
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
        render_execute_review_dialog(frame, state);
        return;
    }
    if let Some(params) = execute_review_params(state) {
        render_modal_confirm(frame, params);
    }
}

/// Width of the single-target execution dialog, as a percentage of the terminal.
const EXECUTE_REVIEW_DIALOG_WIDTH: u16 = 68;
/// Height of the single-target execution dialog, as a percentage of the terminal.
const EXECUTE_REVIEW_DIALOG_HEIGHT: u16 = 56;
/// Header rows: the review, the finding, an optional step, and target files.
const EXECUTE_REVIEW_HEADER_HEIGHT: u16 = 4;
/// Footer rows: the action bar (2) plus the docked hint strip (1).
const EXECUTE_REVIEW_FOOTER_HEIGHT: u16 = 3;
/// Minimum height kept for the directives editor on short terminals.
const EXECUTE_REVIEW_EDITOR_MIN_HEIGHT: u16 = 3;
/// Maximum Coder rows drawn before the list scrolls.
const EXECUTE_REVIEW_MODEL_LIST_MAX_ROWS: u16 = 6;

/// Full drawn confirm-button label, resolved from the runtime locale (c-1).
fn execute_review_confirm_label() -> String {
    format!(" [ {} ] ", active_t("dialog.button.resolve"))
}

/// Full drawn cancel-button label, resolved from the runtime locale (c-1).
fn execute_review_cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Number of Coder rows drawn for `entry_count` entries, before borders.
fn execute_review_model_list_rows(entry_count: usize) -> u16 {
    (entry_count.max(1) as u16).min(EXECUTE_REVIEW_MODEL_LIST_MAX_ROWS)
}

/// Insets a rectangle by a one-cell border on every side.
fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

/// Rectangles of the single-target execution dialog, shared by the renderer and
/// the mouse hit-tester so the surface that is drawn is the surface that is
/// clicked (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteReviewDialogGeometry {
    /// Whole dialog frame.
    pub area: Rect,
    /// Inner rows of the Coder model list (one row per entry).
    pub model_rows: Rect,
    /// The directives editor block.
    pub editor_area: Rect,
    /// Confirm button.
    pub confirm_button: Rect,
    /// Cancel button.
    pub cancel_button: Rect,
}

/// Splits the dialog interior into
/// `[header, coder list, editor, buttons, footer]`.
///
/// The footer, list, and header are sized first so the dialog stays usable on
/// short terminals; the directives editor absorbs whatever height remains and
/// the hint strip is pinned to the card's bottom edge.
fn execute_review_dialog_chunks(area: Rect, entry_count: usize) -> [Rect; 5] {
    let inner = inset(area);

    let footer_h = EXECUTE_REVIEW_FOOTER_HEIGHT.min(inner.height);
    let after_footer = inner.height - footer_h;

    let list_h = (execute_review_model_list_rows(entry_count) + 2).min(after_footer / 2);
    let after_list = after_footer - list_h;

    let header_h = EXECUTE_REVIEW_HEADER_HEIGHT
        .min(after_list.saturating_sub(EXECUTE_REVIEW_EDITOR_MIN_HEIGHT));
    let editor_h = after_list - header_h;

    let header = Rect::new(inner.x, inner.y, inner.width, header_h);
    let list = Rect::new(inner.x, inner.y + header_h, inner.width, list_h);
    let editor = Rect::new(inner.x, list.y + list_h, inner.width, editor_h);

    let docked_h = 1.min(footer_h);
    let buttons_h = footer_h - docked_h;
    let buttons = Rect::new(inner.x, editor.y + editor_h, inner.width, buttons_h);
    let footer = Rect::new(inner.x, buttons.y + buttons_h, inner.width, docked_h);
    [header, list, editor, buttons, footer]
}

/// Computes the dialog frame, the Coder-row area, and the action buttons from
/// the same layout the renderer uses (Conventions §6, §7).
pub fn execute_review_dialog_geometry(
    term: Rect,
    entry_count: usize,
) -> ExecuteReviewDialogGeometry {
    let area = crate::ui::centered_rect(
        EXECUTE_REVIEW_DIALOG_WIDTH,
        EXECUTE_REVIEW_DIALOG_HEIGHT,
        term,
    );
    let chunks = execute_review_dialog_chunks(area, entry_count);
    let model_rows = inset(chunks[1]);
    let editor_area = chunks[2];

    let buttons = chunks[3];
    // Button widths are measured from the same localized labels the renderer
    // draws, so hit-testing never desynchronizes from the drawing.
    let confirm_w = execute_review_confirm_label().chars().count() as u16;
    let cancel_w = execute_review_cancel_label().chars().count() as u16;
    let gap = 4u16;
    let total = confirm_w + gap + cancel_w;
    let start_x = buttons.x + (buttons.width.saturating_sub(total)) / 2;
    let confirm_button = Rect::new(start_x, buttons.y, confirm_w, 1);
    let cancel_button = Rect::new(start_x + confirm_w + gap, buttons.y, cancel_w, 1);

    ExecuteReviewDialogGeometry {
        area,
        model_rows,
        editor_area,
        confirm_button,
        cancel_button,
    }
}

/// Renders the single-target review-execution confirmation.
///
/// Point execution exposes the Coder ladder and a directives editor, so the
/// developer picks the implementing model and adds instructions before a bounded
/// turn spends tokens. The layout mirrors the Architect refinement dialogs —
/// header, embedded model list, editor, action bar, docked hint strip — so both
/// pickers read identically (concept #c-3, Conventions §6, §7).
pub fn render_execute_review_dialog(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = state.confirm_execute_review.as_ref() else {
        return;
    };

    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let entry_count = target.selector.len();
    let area = crate::ui::centered_rect(
        EXECUTE_REVIEW_DIALOG_WIDTH,
        EXECUTE_REVIEW_DIALOG_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", active_t("dialog.execute_review.title_point")))
        .borders(Borders::ALL)
        .border_style(ModalTone::Info.border_style(&engine))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let [header_area, list_area, editor_area, buttons_area, footer_area] =
        execute_review_dialog_chunks(area, entry_count);

    // Header: which review, finding, and step are about to be resolved.
    let mut review_spans = vec![
        Span::styled(active_t("dialog.execute_review.review_label"), dim),
        Span::styled(
            target.review_title.clone(),
            engine.fg(palette.text_primary).bold(),
        ),
    ];
    if let Some(review_id) = target.review_id.as_deref() {
        review_spans.push(Span::styled(format!(" [{}]", review_id), dim));
    }
    let mut header_lines = vec![Line::from(review_spans)];

    if let Some(item) = target.items.first() {
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_review.finding_label"), dim),
            Span::styled(
                format!("#{} {}", item.id, truncate_to_width(&item.title, 44)),
                engine.fg(palette.text_primary).bold(),
            ),
        ]));
        if let Some(step_id) = target.step_id.as_deref() {
            header_lines.push(Line::from(vec![
                Span::styled(active_t("dialog.execute_review.step_label"), dim),
                Span::styled(
                    step_id.to_string(),
                    engine.fg(palette.status_warning).bold(),
                ),
            ]));
        }
        if !item.files.is_empty() {
            header_lines.push(Line::from(vec![
                Span::styled(active_t("dialog.execute_review.files_label"), dim),
                Span::styled(
                    truncate_to_width(&item.files.join(", "), 58),
                    engine.fg(palette.status_info),
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Coder list. The shared `ModelSelector` owns the rows, numbering, scrolling,
    // and empty-list safety; the block adds framing and the focus color.
    let list_title = if entry_count > 1 {
        active_t("dialog.execute_review.model_list_counter")
            .replacen("{}", &(target.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.execute_review.model_list_title")
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
                active_t("dialog.execute_review.no_models"),
                dim,
            ))),
            list_inner,
        );
    } else {
        let rows = target.selector.render_list_lines(list_inner.height as usize);
        frame.render_widget(Paragraph::new(rows), list_inner);
    }

    // Directives editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = target.focus.is_instructions_editor();
    let mut editor_content = Vec::new();
    if target.instructions_editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.execute_review.editor_placeholder"),
            dim,
        ));
        editor_content.push(Line::from(spans));
    } else {
        for line in super::editor_lines(&target.instructions_editor, show_cursor) {
            let mut spans = vec![Span::raw("> ")];
            spans.extend(line.spans);
            editor_content.push(Line::from(spans));
        }
    }
    let editor_border = if target.focus.is_instructions_editor() {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(
            " {} ",
            active_t("dialog.execute_review.editor_title")
        ))
        .borders(Borders::ALL)
        .border_style(editor_border)
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(
        Paragraph::new(editor_content)
            .block(editor_block)
            .wrap(Wrap { trim: false }),
        editor_area,
    );

    // Action bar with focus-aware styling.
    let in_actions = target.focus.is_actions();
    let confirm_selected = target.confirm_button == crate::app::ConfirmDialogButton::Confirm;
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
        Span::styled(execute_review_confirm_label(), confirm_style),
        Span::raw("    "),
        Span::styled(execute_review_cancel_label(), cancel_style),
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
        active_t("dialog.execute_review.hint_actions")
    } else {
        active_t("dialog.execute_review.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    crate::components::modal::render_dialog_footer(frame, footer_area, &nav_hint);
}

/// Presentation parameters for the plan-step execution confirmation.
pub fn execute_scope_params(state: &AppState) -> Option<ConfirmModalParams<'_>> {
    let target = state.confirm_execute_scope.as_ref()?;
    let engine = current_engine();
    let palette = engine.palette();

    let is_single = target.steps.len() == 1;
    let title = if is_single {
        active_t("dialog.execute_scope.title_single")
    } else {
        active_t("dialog.execute_scope.title_scope")
    };

    let heading = if is_single {
        active_t("dialog.execute_scope.heading_single")
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
            Span::styled(format!(" [{}]", target.plan_id), engine.fg(palette.text_muted)),
        ]),
        Line::from(vec![
            Span::styled(
                active_t("dialog.execute_scope.scope_label"),
                engine.fg(palette.text_muted),
            ),
            Span::styled(&target.scope_title, engine.fg(palette.status_warning).bold()),
            Span::styled(
                active_t("dialog.execute_scope.scope_count")
                    .replacen("{}", &target.steps.len().to_string(), 1),
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
                Span::styled(format!("#{} {}", step.id, truncate_to_width(&step.title, 40)), engine.fg(palette.text_primary)),
            ]));
        }
        if target.steps.len() > 4 {
            body_lines.push(Line::from(Span::styled(
                active_t("dialog.execute_scope.queue_more")
                    .replacen("{}", &(target.steps.len() - 4).to_string(), 1),
                engine.fg(palette.text_muted).italic(),
            )));
        }
    }

    body_lines.push(Line::raw(""));
    body_lines.push(Line::from(Span::styled(
        active_t("dialog.execute_scope.note"),
        engine.fg(palette.text_muted),
    )));
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
            Span::styled(format!("{} ({})", plan_title, plan_id), engine.fg(palette.status_info).bold()),
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
            active_t("dialog.delete_review.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw(active_t("dialog.delete_review.review_label")),
            Span::styled(format!("{} ({})", review_title, session_id), engine.fg(palette.status_info).bold()),
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

const BATCH_CANCEL_WIDTH: u16 = 64;
const BATCH_CANCEL_GAP: u16 = 2;

/// Action-button labels of the batch-cancellation dialog, in left-to-right
/// order, resolved from the locale. The geometry and the renderer derive their
/// layout from this single list so the drawn buttons are always the clickable
/// ones (Conventions §6, §9).
fn batch_cancel_labels() -> [String; 3] {
    [
        format!(" {} ", active_t("modal.continue")),
        format!(" {} ", active_t("modal.soft_stop")),
        format!(" {} ", active_t("modal.hard_stop")),
    ]
}

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
            active_t("dialog.batch_cancel.question"),
            engine.fg(palette.status_warning).bold(),
        )),
        Line::raw(""),
        Line::raw(active_t("dialog.batch_cancel.continue_note")),
        Line::raw(active_t("dialog.batch_cancel.soft_stop_note")),
        Line::from(Span::styled(
            active_t("dialog.batch_cancel.hard_stop_note"),
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

    let labels = batch_cancel_labels();
    let widths: [u16; 3] = [
        labels[0].chars().count() as u16,
        labels[1].chars().count() as u16,
        labels[2].chars().count() as u16,
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
/// the configured modifiers; the fast-select accelerators are the batch's own
/// `Continue` / `Soft Stop` / `Hard Stop` keys, so they are named here rather
/// than in the universal dialog map.
fn batch_cancel_hint() -> String {
    format!(
        "{} • s / h / c {} • {}",
        crate::components::modal::dialog_navigation_hint(),
        active_t("dialog.action.fast_select"),
        crate::components::modal::dialog_dismiss_hint(),
    )
}

/// Renders the three-way batch-cancellation dialog.
pub fn render_confirm_batch_cancel_popup(frame: &mut ratatui::Frame, state: &AppState) {
    let engine = current_engine();
    let palette = engine.palette();
    let geom = batch_cancel_modal_geometry(frame.area());

    let title = active_t("dialog.batch_cancel.title");
    let inner_area = crate::components::modal::render_modal_container(
        frame,
        geom.area,
        &title,
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

    let [l0, l1, l2] = batch_cancel_labels();
    let focused = state.batch_cancel_button;
    let button_style = |button: BatchCancelButton| {
        if button == focused {
            engine.bg(palette.button_active_bg).fg(engine.color(palette.text_inverse)).bold()
        } else {
            engine.bg(palette.button_bg).fg(engine.color(palette.button_fg))
        }
    };

    let actions_line = Line::from(vec![
        Span::styled(l0, button_style(BatchCancelButton::Continue)),
        Span::raw("  "),
        Span::styled(l1, button_style(BatchCancelButton::SoftStop)),
        Span::raw("  "),
        Span::styled(l2, button_style(BatchCancelButton::HardStop)),
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
