//! Plan step and scope execution dialog: Coder picker and directives editor.
//!
//! Executing a plan step or an implementation scope hands control to the Coder
//! role, so the confirmation exposes the configured Coder ladder and a
//! directives editor before any bounded turn spends tokens. The layout mirrors
//! the single-target review execution dialog — header, embedded model list,
//! editor, action bar, docked hint strip — so both pickers read identically
//! (c-tui-selector, Conventions §6, §7).

use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{AppState, ConfirmDialogButton, ConfirmExecuteScopeState};
use crate::components::modal::verb_hint;
use crate::i18n::active_t;
use crate::theme::engine::current_engine;
use crate::ui::header::truncate_to_width;

/// Width of the execution dialog, as a percentage of the terminal.
const EXECUTE_SCOPE_DIALOG_WIDTH: u16 = 68;
/// Height of the execution dialog, as a percentage of the terminal.
const EXECUTE_SCOPE_DIALOG_HEIGHT: u16 = 56;
/// Header rows: the plan, the scope, and the targeted step or queue preview.
const EXECUTE_SCOPE_HEADER_HEIGHT: u16 = 4;

/// Whether the pending execution targets exactly one scheduled step.
fn is_single_step(target: &ConfirmExecuteScopeState) -> bool {
    target.steps.len() == 1
}

/// Dialog title naming the shape of the pending execution.
///
/// A concept-only plan carries no scheduled leaf yet, so the title states that
/// its first horizon is projected just-in-time instead of presenting an empty
/// queue as a finished scope (c-tui-selector).
fn dialog_title(target: &ConfirmExecuteScopeState) -> String {
    if is_single_step(target) {
        active_t("dialog.execute_scope.title_single")
    } else if target.steps.is_empty() {
        active_t("dialog.execute_scope.title_jit")
    } else {
        active_t("dialog.execute_scope.title_scope")
    }
}

/// Full drawn confirm-button label, resolved from the runtime locale (c-1).
fn confirm_label(single_step: bool) -> String {
    let verb = if single_step {
        active_t("dialog.button.execute")
    } else {
        active_t("dialog.button.execute_scope")
    };
    format!(" [ {} ] ", verb)
}

/// Full drawn cancel-button label, resolved from the runtime locale (c-1).
fn cancel_label() -> String {
    format!(" [ {} ] ", active_t("modal.cancel"))
}

/// Rectangles of the execution dialog, shared by the renderer and the mouse
/// hit-tester so the surface that is drawn is the surface that is clicked
/// (Conventions §6, §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecuteScopeDialogGeometry {
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

/// Computes the dialog frame, the Coder-row area, and the action buttons from
/// the shared picker layout the renderer uses, so the drawn buttons are always
/// the clickable ones (Conventions §6, §7).
pub fn execute_scope_dialog_geometry(
    term: Rect,
    entry_count: usize,
    single_step: bool,
) -> ExecuteScopeDialogGeometry {
    let area = crate::ui::centered_rect(
        EXECUTE_SCOPE_DIALOG_WIDTH,
        EXECUTE_SCOPE_DIALOG_HEIGHT,
        term,
    );
    let chunks = super::picker_dialog_chunks(area, entry_count, EXECUTE_SCOPE_HEADER_HEIGHT, 0);

    // The button geometry is measured from the same localized labels the
    // renderer draws, so hit-testing never desynchronizes from the drawing.
    let (confirm_button, cancel_button) =
        super::picker_button_rects(chunks.buttons, &confirm_label(single_step), &cancel_label());

    ExecuteScopeDialogGeometry {
        area,
        model_rows: super::inset(chunks.list),
        editor_area: chunks.editor,
        confirm_button,
        cancel_button,
    }
}

/// Renders the plan step or scope execution confirmation.
///
/// The developer picks the starting Coder rung and may add implementation
/// directives; both travel on the wire, so the coder that runs is the coder
/// shown on screen and the server never re-derives the selection
/// (c-tui-selector, Conventions §6, §7).
pub fn render_execute_scope_dialog(frame: &mut ratatui::Frame, state: &AppState) {
    let Some(target) = state.confirm_execute_scope.as_ref() else {
        return;
    };

    let engine = current_engine();
    let palette = engine.palette();
    let dim = engine.fg(palette.text_muted);
    let single = is_single_step(target);
    let entry_count = target.selector.len();
    let area = crate::ui::centered_rect(
        EXECUTE_SCOPE_DIALOG_WIDTH,
        EXECUTE_SCOPE_DIALOG_HEIGHT,
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let outer_block = Block::default()
        .title(format!(" {} ", dialog_title(target)))
        .borders(Borders::ALL)
        .border_style(engine.fg(palette.button_active_bg))
        .style(Style::default().bg(engine.color(palette.surface)));
    frame.render_widget(outer_block, area);

    let super::PickerDialogChunks {
        header: header_area,
        list: list_area,
        editor: editor_area,
        buttons: buttons_area,
        footer: footer_area,
        ..
    } = super::picker_dialog_chunks(area, entry_count, EXECUTE_SCOPE_HEADER_HEIGHT, 0);

    // Header: which plan and scope are about to be executed.
    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(active_t("dialog.execute_scope.plan_label"), dim),
            Span::styled(
                target.plan_title.clone(),
                engine.fg(palette.text_primary).bold(),
            ),
            Span::styled(format!(" [{}]", target.plan_id), dim),
        ]),
        Line::from(vec![
            Span::styled(active_t("dialog.execute_scope.scope_label"), dim),
            Span::styled(
                target.scope_title.clone(),
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

    if single {
        let step = &target.steps[0];
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_scope.step_label"), dim),
            Span::styled(
                format!("#{} {}", step.id, truncate_to_width(&step.title, 40)),
                engine.fg(palette.text_primary).bold(),
            ),
        ]));
    } else if target.steps.is_empty() {
        // A concept-only plan projects its first horizon just-in-time, so the
        // dialog states that before the queue starts (c-tui-selector).
        header_lines.push(Line::from(Span::styled(
            truncate_to_width(&active_t("dialog.execute_scope.jit_note"), 62),
            engine.fg(palette.text_secondary),
        )));
    } else {
        let step = &target.steps[0];
        header_lines.push(Line::from(vec![
            Span::styled(active_t("dialog.execute_scope.queue_header"), dim),
            Span::styled(
                format!("#{} {}", step.id, truncate_to_width(&step.title, 36)),
                engine.fg(palette.text_primary),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(header_lines), header_area);

    // Coder list. The shared `ModelSelector` owns the rows, numbering, scrolling,
    // and empty-list safety; the block adds framing and the focus color.
    let list_title = if entry_count > 1 {
        active_t("dialog.execute_scope.model_list_counter")
            .replacen("{}", &(target.selector.selected_index() + 1).to_string(), 1)
            .replacen("{}", &entry_count.to_string(), 1)
    } else {
        active_t("dialog.execute_scope.model_list_title")
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
                active_t("dialog.execute_scope.no_models"),
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

    // Directives editor. The cursor is drawn only when the editor owns focus.
    let show_cursor = target.focus.is_instructions_editor();
    let mut editor_content = Vec::new();
    if target.instructions_editor.is_empty() {
        let mut spans = vec![Span::raw("> ")];
        if show_cursor {
            spans.push(Span::styled("█", engine.fg(palette.status_warning)));
        }
        spans.push(Span::styled(
            active_t("dialog.execute_scope.editor_placeholder"),
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
    let editor_border = if show_cursor {
        engine.fg(palette.status_success).bold()
    } else {
        engine.fg(palette.border)
    };
    let editor_block = Block::default()
        .title(format!(
            " {} ",
            active_t("dialog.execute_scope.editor_title")
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
        Span::styled(confirm_label(single), confirm_style),
        Span::raw("    "),
        Span::styled(cancel_label(), cancel_style),
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
        active_t("dialog.execute_scope.hint_actions")
    } else {
        active_t("dialog.execute_scope.hint_fields")
            .replacen("{}", primary.prefix_str(), 1)
            .replacen(
                "{}",
                &crate::components::model_selector::model_select_hint(primary),
                1,
            )
    };
    crate::components::modal::render_dialog_footer(frame, footer_area, &verb_hint(nav_hint));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tauqe_protocol::{ModelEntry, ModelRef, PlanItem, PlanItemStatus};

    fn scope_state(coder_entries: Vec<ModelEntry>) -> AppState {
        let mut st = crate::app::test_state();
        st.confirm_execute_scope = Some(ConfirmExecuteScopeState::new(
            "plan-1".to_string(),
            "Plan One".to_string(),
            "Step One".to_string(),
            Some("1".to_string()),
            vec![PlanItem {
                id: "1".to_string(),
                title: "Step One".to_string(),
                status: PlanItemStatus::Todo,
                ..Default::default()
            }],
            coder_entries,
        ));
        st
    }

    /// The geometry used for mouse hit-testing must land exactly on the drawn
    /// action buttons, whatever the localized labels and the body shape are
    /// (Conventions §6, §7).
    #[test]
    fn rendered_scope_buttons_match_hit_geometry() {
        let term = Rect::new(0, 0, 100, 40);
        let st = scope_state(vec![ModelEntry::new(ModelRef::openrouter("coder-a"), None)]);
        let target = st.confirm_execute_scope.as_ref().expect("dialog open");
        let entry_count = target.selector.len();
        let single = target.steps.len() == 1;

        let mut terminal = Terminal::new(TestBackend::new(term.width, term.height)).unwrap();
        terminal
            .draw(|f| render_execute_scope_dialog(f, &st))
            .unwrap();

        let geom = execute_scope_dialog_geometry(term, entry_count, single);
        let buffer = terminal.backend().buffer();
        let read = |rect: Rect| -> String {
            (rect.x..rect.x + rect.width)
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };

        assert!(
            read(geom.confirm_button).contains("Execute"),
            "confirm row: {:?}",
            read(geom.confirm_button)
        );
        assert!(
            read(geom.cancel_button).contains("Cancel"),
            "cancel row: {:?}",
            read(geom.cancel_button)
        );
    }
}
